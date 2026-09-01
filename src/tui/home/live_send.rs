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

/// Distinguishes successive live-send workers in the size-owner lock, so a
/// rapid live-mode toggle's old worker can't release a lock the new worker
/// already re-stole. Process-local; combined with the pid for cross-process
/// uniqueness.
static LIVE_SEND_WORKER_COUNTER: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
#[cfg(test)]
static LIVE_CAPTURE_WORKER_TEST_COUNTER: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

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
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(in crate::tui) enum LiveSendTarget {
    /// The agent's tmux pane (default, pre-existing behavior).
    #[default]
    Agent,
    /// The paired host-shell terminal pane.
    Terminal,
    /// The paired container-shell terminal pane (sandboxed sessions
    /// in container terminal mode).
    ContainerTerminal,
    /// A named tool's paired tmux pane.
    Tool(String),
}

/// Format a display label for a `(title, target)` pair so the compose
/// dialog header and the live-mode status banner stay in lockstep.
/// Agent keeps the bare title (historical look); terminal variants
/// get a short parenthetical so the user sees which pane the
/// keystrokes will land on without having to read the preview chrome.
pub(in crate::tui) fn format_target_label(title: &str, target: &LiveSendTarget) -> String {
    match target {
        LiveSendTarget::Agent => title.to_string(),
        LiveSendTarget::Terminal => format!("{title} (terminal)"),
        LiveSendTarget::ContainerTerminal => format!("{title} (container)"),
        LiveSendTarget::Tool(name) => format!("{title} ({name})"),
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
    /// `send-keys -N <count> <name>`: the named key repeated `count` times
    /// in one fork. Consecutive runs of the same key (e.g. several wheel
    /// notches drained in one batch) fold their counts together.
    NamedRepeat {
        name: String,
        count: usize,
    },
    HexBytes(Vec<u8>),
    /// A multi-line paste routed through `paste-buffer -p`.
    Paste(String),
    Resize {
        cols: u16,
        rows: u16,
    },
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
            WorkerMsg::Send(TmuxKey::NamedRepeat { name, count }) => {
                flush(&mut out, &mut run);
                match out.last_mut() {
                    Some(TmuxAction::NamedRepeat {
                        name: prev_name,
                        count: prev_count,
                    }) if *prev_name == name => *prev_count += count,
                    _ => out.push(TmuxAction::NamedRepeat { name, count }),
                }
            }
            WorkerMsg::Send(TmuxKey::HexBytes(bytes)) => {
                flush(&mut out, &mut run);
                match out.last_mut() {
                    Some(TmuxAction::HexBytes(prev)) => prev.extend_from_slice(&bytes),
                    _ => out.push(TmuxAction::HexBytes(bytes)),
                }
            }
            WorkerMsg::Send(TmuxKey::Paste(text)) => {
                // Never merged: a paste is one discrete tmux paste-buffer
                // call, and folding it into a neighbouring run would put the
                // payload back on the literal path the markers came from.
                flush(&mut out, &mut run);
                out.push(TmuxAction::Paste(text));
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

/// Whether a drained batch must verify size ownership BEFORE dispatch.
/// Only geometry changes need that ordering: a `resize-window` racing
/// another surface's live grid is the flap the size-owner lock exists to
/// kill. Plain keystrokes never read the lock, so they dispatch without
/// waiting on the verify's tmux forks.
pub(super) fn batch_needs_owner_first(batch: &[WorkerMsg]) -> bool {
    batch.iter().any(|m| matches!(m, WorkerMsg::Resize { .. }))
}

fn resize_dispatch_authorized(owned: bool, lock_lost: bool) -> bool {
    owned && !lock_lost
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
    /// Set (sticky) by the worker thread when the size-owner lock is
    /// observed held by another surface. The UI loop polls this each tick
    /// and exits live mode; the worker itself never steals the lock back
    /// after entry, so a web "take over" wins instead of ping-ponging.
    lock_lost: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Set by the worker when resize-window fails or times out. Paint consumes
    /// the flag and schedules a bounded retry for the same geometry.
    resize_failed: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl LiveSendWorker {
    /// `capture_wake`, when present, nudges the preview capture worker out
    /// of its inter-capture wait right after each dispatched keystroke
    /// batch, so the typed echo is captured immediately instead of waiting
    /// up to a full fast-cadence cycle. That ties echo latency to actual
    /// input rather than the background capture phase.
    pub(super) fn spawn(tmux_name: String, capture_wake: Option<LiveCaptureWake>) -> Self {
        let (tx, rx) = channel::<WorkerMsg>();
        let lock_lost = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let thread_lock_lost = std::sync::Arc::clone(&lock_lost);
        let resize_failed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let thread_resize_failed = std::sync::Arc::clone(&resize_failed);

        std::thread::spawn(move || {
            use std::sync::atomic::Ordering;
            use std::sync::mpsc::RecvTimeoutError;

            // TUI live-send is an active take-over: entering live mode steals
            // the session's size so the web PTY relay and the mobile live
            // view defer to it. Entry is the ONLY steal: afterwards
            // ownership is merely refreshed, and losing it (a web "take
            // over" tap, another TUI's live entry) flips `lock_lost` so the
            // UI exits live mode instead of fighting the new owner.
            // Re-stealing after entry is how a background TUI used to
            // silently revert a phone takeover: any keystroke, or any
            // preview-rect jitter (a one-frame toast, a divider drag)
            // re-asserted this worker's grid and yanked the pane size back.
            // Released (and `window-size latest` restored) when live mode
            // exits and the channel closes; on a lost lock the release is a
            // no-op and the new owner's sizing stands.
            let owner_id = format!(
                "tui-{}-{}",
                std::process::id(),
                LIVE_SEND_WORKER_COUNTER.fetch_add(1, Ordering::Relaxed)
            );
            let session = crate::tmux::Session::from_name(&tmux_name);
            // Entering live mode is explicit user intent, so entry forces the
            // lock even over a live holder. False means either the tmux
            // session is missing/broken at entry or another surface won the
            // confirm-read race; the retry on the next resize batch tells
            // those apart (a slow-to-appear pane still gets owned, a real
            // takeover is flagged instead of fought).
            let mut owned = session.steal_size_owner(&owner_id);
            // Refresh-or-flag: bump our heartbeat iff we still hold the
            // lock; a failed refresh means another surface took over, which
            // is flagged once and never fought.
            let maintain = |owned: bool| -> bool {
                if !owned || thread_lock_lost.load(Ordering::Relaxed) {
                    return false;
                }
                let still_owner = session.refresh_size_owner(&owner_id);
                if !still_owner {
                    thread_lock_lost.store(true, Ordering::Relaxed);
                }
                still_owner
            };

            // Block (up to a heartbeat) for the first message, then drain
            // anything else that piled up. The drain plus `coalesce` collapses
            // paste-bursts and held-key autorepeat into one fork per literal
            // run, so typing a long sentence costs one `tmux send-keys -l`
            // invocation, not one per character.
            //
            // Owner bookkeeping stays OFF the keystroke critical path: a
            // steal is ~5 tmux forks (3-13ms each on macOS) and used to run
            // ahead of every batch's byte write, which made typing in live
            // mode measurably laggier than a direct tmux attach. Keystrokes
            // never read the size lock, so they dispatch first and ownership
            // is re-asserted at most once per heartbeat afterwards. Resize
            // batches are the exception: geometry must not race another
            // owner's grid, so they keep the steal-before-dispatch ordering.
            let mut last_owner_maintenance = std::time::Instant::now();
            loop {
                match rx.recv_timeout(crate::tmux::SIZE_OWNER_HEARTBEAT) {
                    Ok(first) => {
                        let mut batch = vec![first];
                        while let Ok(msg) = rx.try_recv() {
                            batch.push(msg);
                        }
                        // Geometry must not race another owner's grid, so a
                        // batch carrying a resize VERIFIES ownership first;
                        // plain keystrokes never read the lock.
                        let mut resize_unverified = false;
                        if batch_needs_owner_first(&batch) {
                            if !owned {
                                // Entry steal failed. Claim rather than steal:
                                // a vacant, stale, or already-ours lock still
                                // gets taken (the slow-to-appear pane case),
                                // but a live holder means another surface won
                                // the lock and must not be stomped. Re-forcing
                                // here would fight a takeover the entry race
                                // makes indistinguishable from a missing pane,
                                // and would never flag the loss.
                                owned = session
                                    .claim_size_owner(&owner_id, crate::tmux::SIZE_OWNER_TTL);
                                if !owned {
                                    match session.has_active_size_owner() {
                                        Some(true) => {
                                            // Our own claim would have succeeded if the
                                            // live owner were us, so this is a takeover.
                                            thread_lock_lost.store(true, Ordering::Relaxed);
                                        }
                                        Some(false) | None => resize_unverified = true,
                                    }
                                }
                            } else {
                                maintain(owned);
                            }
                            last_owner_maintenance = std::time::Instant::now();
                        }
                        let lock_lost = thread_lock_lost.load(Ordering::Relaxed);
                        if !resize_dispatch_authorized(owned, lock_lost) {
                            // A resize without verified ownership could stomp
                            // another surface's grid. Keys remain independent
                            // of the size lock and still deliver in order.
                            batch.retain(|m| !matches!(m, WorkerMsg::Resize { .. }));
                            if resize_unverified {
                                thread_resize_failed.store(true, Ordering::Relaxed);
                            }
                        }
                        if !batch.is_empty() {
                            match dispatch_batch(&tmux_name, &owner_id, batch) {
                                ResizeDispatchResult::Failed => {
                                    owned = false;
                                    thread_resize_failed.store(true, Ordering::Relaxed);
                                }
                                ResizeDispatchResult::Succeeded => {
                                    thread_resize_failed.store(false, Ordering::Relaxed);
                                }
                                ResizeDispatchResult::None => {}
                            }
                            if let Some(wake) = &capture_wake {
                                wake.wake();
                            }
                        }

                        if last_owner_maintenance.elapsed() >= crate::tmux::SIZE_OWNER_HEARTBEAT
                            && maintain(owned)
                        {
                            last_owner_maintenance = std::time::Instant::now();
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        // Idle heartbeat. A failed refresh here is usually
                        // the earliest takeover signal (the web steals while
                        // the desktop sits idle); `maintain` flags it so the
                        // UI can exit live mode without waiting for input.
                        if maintain(owned) {
                            last_owner_maintenance = std::time::Instant::now();
                        }
                    }
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
            session.release_size_owner(&owner_id);
        });
        Self {
            tx,
            lock_lost,
            resize_failed,
        }
    }

    /// True once the worker observed the size-owner lock held by another
    /// surface. Sticky for the worker's lifetime; the UI loop polls it and
    /// exits live mode.
    pub(super) fn lock_lost(&self) -> bool {
        self.lock_lost.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Consume the sticky resize failure so paint can schedule a bounded
    /// retry for the same geometry.
    pub(super) fn take_resize_failed(&self) -> bool {
        self.resize_failed
            .swap(false, std::sync::atomic::Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(super) fn force_lock_lost_for_test(&self) {
        self.lock_lost
            .store(true, std::sync::atomic::Ordering::Relaxed);
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
/// Capture cadence while live-send is attached to the displayed pane: tight
/// so typed echo and agent output appear with near-attach latency. On the VT
/// path this is only the fallback wait: the channel's change wakeup ends the
/// sleep the moment output lands, and this value doubles as the floor between
/// *published* frames so change-driven sampling can't outpace the frame
/// pacing the 33ms render ticker was calibrated against.
const LIVE_CAPTURE_INTERVAL_FAST_MS: u64 = 25;

/// Milliseconds a just-changed frame must still wait before it may publish,
/// given how long ago the previous frame published (`None` = never). Zero
/// means publish now: the first change after a quiet gap always goes out
/// immediately (this is the typed-echo case), while sustained streaming
/// paces at the fast cadence, matching the old fixed cycle. The floor
/// keys off publishes, not samples, so a wasted pre-echo sample (the send
/// worker's wake fires before the agent has echoed) can't push the real
/// echo back a cycle.
pub(super) fn publish_floor_wait_ms(since_last_publish_ms: Option<u64>) -> u64 {
    match since_last_publish_ms {
        None => 0,
        Some(elapsed) => LIVE_CAPTURE_INTERVAL_FAST_MS.saturating_sub(elapsed),
    }
}

/// Quiescence window for the VT sample debounce: while output is streaming in
/// back-to-back chunks, a changed frame waits until the stream has been silent
/// for this long before it may publish, so a clear-then-reprint that spans
/// several chunks publishes once it settles instead of mid-repaint (the flash
/// in #2903). Small enough that the settled frame still lands promptly once
/// output stops.
const SAMPLE_QUIESCENCE_MS: u64 = 6;

/// Hard cap on how long the sample debounce may hold a changed frame. Sustained
/// output (a stream that never goes quiet) hits this and publishes anyway, so
/// heavy streaming still renders at a bounded cadence rather than stalling
/// until it happens to pause.
const SAMPLE_LATENCY_CAP_MS: u64 = 40;

/// How long a just-changed VT frame must wait before it may sample/publish,
/// given whether output is currently `streaming` (chunks arriving back-to-back
/// within [`SAMPLE_QUIESCENCE_MS`]), `since_last_chunk_ms` (how long ago the
/// most recent chunk landed), and `since_pending_ms` (how long this pending
/// change has been held). Zero means publish now.
///
/// A lone chunk (a keystroke echo, or the first chunk after a quiet gap)
/// reports `streaming == false` and never waits, so the #2822 echo-latency path
/// is untouched. Only a live stream defers, and only until it goes quiet
/// (`since_last_chunk_ms >= SAMPLE_QUIESCENCE_MS`) or the latency cap fires.
pub(super) fn sample_debounce_wait_ms(
    streaming: bool,
    since_last_chunk_ms: u64,
    since_pending_ms: u64,
) -> u64 {
    if !streaming || since_pending_ms >= SAMPLE_LATENCY_CAP_MS {
        return 0;
    }
    let quiescence_remaining = SAMPLE_QUIESCENCE_MS.saturating_sub(since_last_chunk_ms);
    if quiescence_remaining == 0 {
        return 0;
    }
    quiescence_remaining
        .min(SAMPLE_LATENCY_CAP_MS.saturating_sub(since_pending_ms))
        .max(1)
}
/// Capture cadence when the worker is just keeping the home-list preview warm
/// (no live-send). Matches the render-driven throttle it replaces, so moving
/// the fork off the render thread doesn't raise the idle fork rate.
const LIVE_CAPTURE_INTERVAL_IDLE_MS: u64 = 250;
/// Maximum interval between authoritative snapshots while a live VT grid is
/// otherwise supplying preview frames. This preserves upstream's bounded
/// self-heal for a grid whose cells diverge while cursor and geometry agree,
/// without moving `capture-pane` back onto paint.
const AUTHORITATIVE_CAPTURE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);
const AUTHORITATIVE_REFRESH_QUIESCENCE_MS: u64 = LIVE_CAPTURE_INTERVAL_FAST_MS * 2;

fn authoritative_capture_due(
    next_attempt: Option<std::time::Instant>,
    now: std::time::Instant,
) -> bool {
    next_attempt.is_none_or(|at| now >= at)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthoritativeRefreshAction {
    None,
    Schedule(std::time::Duration),
}

fn authoritative_refresh_action(
    due: bool,
    result: Option<crate::tmux::vt::VtRefreshResult>,
) -> AuthoritativeRefreshAction {
    match (due, result) {
        (false, _) => AuthoritativeRefreshAction::None,
        // Busy still consumed an authoritative capture attempt. Defer the
        // next one so continuous output cannot turn self-heal into a fork loop.
        (
            true,
            Some(
                crate::tmux::vt::VtRefreshResult::Busy
                | crate::tmux::vt::VtRefreshResult::Refreshed,
            ),
        ) => AuthoritativeRefreshAction::Schedule(AUTHORITATIVE_CAPTURE_INTERVAL),
        // A failed snapshot does not prove the pipe channel died. Keep sampling
        // its live grid and retry self-heal at the shorter re-arm cadence.
        (true, Some(crate::tmux::vt::VtRefreshResult::Failed) | None) => {
            AuthoritativeRefreshAction::Schedule(VT_REARM_INTERVAL)
        }
    }
}

fn authoritative_refresh_is_quiet(chunk_timing: Option<(u64, u64)>) -> bool {
    chunk_timing
        .is_none_or(|(since_last_ms, _)| since_last_ms >= AUTHORITATIVE_REFRESH_QUIESCENCE_MS)
}
/// Minimum wait between VT arm attempts for one target. A dead channel (the
/// pane was killed and its tmux session recreated under the same name, e.g. a
/// session restart) heals within this window instead of stranding the pane on
/// the capture fallback until a retarget; a permanently un-armable pane costs
/// one cheap failed attempt per interval rather than one per 25ms tick.
const VT_REARM_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3);

/// Cloneable handle that nudges a [`LiveCaptureWorker`] out of its
/// inter-capture wait. Handed to [`LiveSendWorker`] so a dispatched
/// keystroke batch triggers an immediate capture of the typed echo rather
/// than waiting up to a full fast-cadence cycle. Backed by the same condvar
/// `set_live` / `set_target` use, so a wake just runs one capture early.
type CaptureWake = std::sync::Arc<(std::sync::Mutex<u64>, std::sync::Condvar)>;

fn signal_capture_wake(wakeup: &CaptureWake) {
    if let Ok(mut generation) = wakeup.0.lock() {
        *generation = generation.wrapping_add(1);
        wakeup.1.notify_one();
    }
}

fn wait_for_capture_wake(
    wakeup: &CaptureWake,
    observed: &mut u64,
    timeout: std::time::Duration,
) -> bool {
    let previous = *observed;
    let Ok(mut generation) = wakeup.0.lock() else {
        return false;
    };
    let pending_before_park = *generation != previous;
    if !pending_before_park {
        let Ok((next, _)) = wakeup
            .1
            .wait_timeout_while(generation, timeout, |current| *current == previous)
        else {
            return false;
        };
        generation = next;
    }
    *observed = *generation;
    pending_before_park
}

#[derive(Clone)]
pub(in crate::tui) struct LiveCaptureWake {
    nudge: CaptureWake,
}

impl LiveCaptureWake {
    fn wake(&self) {
        signal_capture_wake(&self.nudge);
    }
}

/// One atomic capture frame: content, the pane cursor probed in the same
/// cycle, the line budget it was captured under, and the target generation
/// it belongs to. Published as a single unit so a consumer can never see a
/// frame torn across a retarget or a budget change.
#[derive(Debug)]
pub(in crate::tui) struct CaptureFrame {
    /// Target generation at capture time; compared against the worker's
    /// current generation so a frame captured before a retarget is dropped
    /// instead of landing under the new view.
    pub(in crate::tui) generation: u64,
    /// Exact tmux target identity captured in this frame.
    pub(in crate::tui) target: String,
    /// The capture_lines budget this capture was produced under.
    pub(in crate::tui) budget: usize,
    pub(in crate::tui) content: String,
    pub(in crate::tui) cursor: Option<crate::tmux::PaneCursor>,
}

#[derive(Debug)]
struct ClipboardFrame {
    generation: u64,
    target: String,
    text: String,
}

/// Whether the worker must reset target-scoped dedup and transport state.
/// Generation is part of the identity: A -> B -> A can happen entirely
/// between worker cycles, leaving the name unchanged but the mailbox cleared.
fn capture_target_changed(
    last_name: &str,
    last_generation: u64,
    name: &str,
    generation: u64,
) -> bool {
    last_name != name || last_generation != generation
}

fn frame_needs_publish(
    content_changed: bool,
    last_cursor: Option<crate::tmux::PaneCursor>,
    cursor: Option<crate::tmux::PaneCursor>,
    budget_changed: bool,
) -> bool {
    content_changed || last_cursor != cursor || budget_changed
}
/// Off-thread preview capture. One long-lived thread forks `tmux
/// capture-pane` and publishes fresh pane content into a single-slot
/// mailbox the render loop drains, so the render loop applies the latest
/// content without forking. That moves the per-frame capture cost (~8.5ms
/// measured on macOS, ~90% of a live-send frame) off the hot path.
/// Dropping the worker flips `stop` so the thread exits after its current
/// cycle; like `LiveSendWorker` we don't join.
///
/// Tracks whichever pane the preview is currently displaying (agent,
/// terminal, container shell, or tool), not just the agent: the home view
/// points it via `set_target` (from `sync_preview_capture_worker`) whenever
/// the selected session or view mode changes, so every preview path reads
/// fresh content without ever forking on the render thread, and a switch
/// swaps the target in place instead of spawning a new thread. The capture
/// cadence adapts (`set_live`): tight while live-send is attached,
/// `LIVE_CAPTURE_INTERVAL_IDLE_MS` otherwise so the background preview costs
/// no more idle forks than the render-driven throttle it replaces did.
pub(in crate::tui) struct LiveCaptureWorker {
    /// Lines the render loop wants captured (height + scrollback + buffer).
    /// `0` means "not set yet"; the worker skips capturing until the first
    /// render publishes a real value. `capture_lines_for` never yields 0.
    capture_lines: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// tmux session name the worker is currently capturing. Swapped in
    /// place by `set_target` when the displayed pane changes, so one
    /// long-lived thread serves every view without per-switch respawns.
    /// Empty string means "idle, capture nothing" (no selection).
    target: std::sync::Arc<std::sync::Mutex<String>>,
    /// Single-slot mailbox holding the newest [`CaptureFrame`] not yet
    /// consumed by the render loop. A new frame overwrites an unconsumed one
    /// (the render only ever wants the latest), so this can't grow unbounded
    /// if the render thread stalls.
    latest: std::sync::Arc<std::sync::Mutex<Option<CaptureFrame>>>,
    /// Bumped on every `set_target` change. Published frames carry the value
    /// read at capture time; a consumer drops frames whose generation no
    /// longer matches, so stale bytes can never land under a new view even if
    /// they were captured mid-switch.
    generation: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Sleep between captures, in ms. Adaptive: fast under live-send, idle
    /// otherwise. Read by the worker thread each cycle.
    interval_ms: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Whether live-send is currently attached to this worker's pane. A
    /// FAILED capture means opposite things on the two sides: outside live
    /// mode it surfaces as an empty frame ("session gone"); during live-send
    /// the #1501 kill switch preserves the last-good frame.
    live: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Whether an empty capture should be forwarded (clearing stale preview
    /// text) or dropped (preserving the last-good frame, the #1501 kill
    /// switch). Terminal / container panes forward empties so a cleared
    /// shell doesn't keep showing stale output; agent / tool panes preserve.
    /// Set per target by `set_forward_empty`, read by the worker each cycle.
    forward_empty: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Interrupts the worker's inter-capture wait so a cadence or target
    /// change takes effect immediately instead of after the current (up to
    /// 250ms idle) sleep. Without this, entering live-send mid-idle-sleep
    /// would lag the first fast capture by ~250ms.
    nudge: CaptureWake,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Newest OSC 52 clipboard write the displayed pane has emitted. A VT grid
    /// extracts it from the live byte stream; terminal capture uses a separate
    /// raw observer so rendered snapshots never need to carry the escape.
    clipboard: std::sync::Arc<std::sync::Mutex<Option<ClipboardFrame>>>,
    /// Whether the worker may render through a VT channel (`[tmux] vt_live`).
    /// Pushed by the render reconcile at spawn and on config refresh
    /// (`set_vt_enabled`), read by the worker each cycle: toggling off tears
    /// down an armed channel (disabling its `pipe-pane`) and falls back to
    /// the capture path in place, no restart needed.
    vt_enabled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Whether the raw OSC 52 observer may run when terminal rendering uses
    /// capture-pane. Mirrors the Clipboard Pass-through setting, so disabled
    /// mode does not keep a second pipe-pane connection open.
    clipboard_capture_enabled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Cycle counter, bumped before every deadline-bounded sample. Changed
    /// frames cannot serve as a heartbeat because idle content is deduplicated;
    /// render observes this counter and replaces a worker that stops advancing.
    cycles: std::sync::Arc<std::sync::atomic::AtomicU64>,
    #[cfg(test)]
    test_id: u64,
    #[cfg(test)]
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for LiveCaptureWorker {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        // Wake the worker so it sees `stop` and exits now rather than after
        // its current inter-capture sleep.
        self.nudge();
        #[cfg(test)]
        if let Some(thread) = self.thread.take() {
            if let Err(panic) = thread.join() {
                // Preserve the original test panic instead of aborting on a
                // second panic from cleanup.
                if !std::thread::panicking() {
                    std::panic::resume_unwind(panic);
                }
            }
        }
    }
}
/// How often the worker re-asks how many panes its target window has, and
/// whether one is zoomed. The answer only changes when the user splits, closes,
/// or zooms a pane by hand, so a lazy cadence is enough; it costs one tiny
/// `display-message` fork on the previewed pane, in live mode as well as out of
/// it (a mid-session split has to be noticed either way).
///
/// This bounds a visible transient rather than only a cost. The render-thread
/// fallback probes `window_panes` in the SAME fork as its capture, so it composites
/// as soon as the user splits, while the worker keeps publishing single-pane
/// frames until this elapses; the preview alternates between the two until they
/// agree. One second keeps that wobble short without putting the probe anywhere
/// near per-frame. `VtChannel::sample` already forks `pane_size` about once a
/// second, so this roughly doubles a cost that was already there rather than
/// introducing one.
const PANE_COUNT_PROBE_MS: u64 = 1_000;

/// How many panes the worker's target window has, for deciding whether the
/// preview needs the composite path. Returns 1 on any failure, which keeps the
/// caller on the cheap single-pane transport.
///
/// A zoomed pane (`C-b z`) also reports 1: tmux keeps `window_panes` at its real
/// count while reporting every pane at the window's full rectangle, so the panes
/// overlap and the compositor's tiling assumption does not hold. Compositing
/// there hides the zoomed pane behind border fill, so the single-pane transport
/// is both cheaper and more correct.
fn probe_pane_count(name: &str, deadline: &crate::tmux::TmuxCommandDeadline) -> u16 {
    let mut command = crate::tmux::tmux_command();
    command.args([
        "display-message",
        "-p",
        "-t",
        &format!("{name}:^"),
        "-F",
        "#{window_panes} #{window_zoomed_flag}",
    ]);
    let out = deadline
        .run(&mut command)
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok());
    let Some(out) = out else { return 1 };
    let mut fields = out.split_whitespace();
    let count: u16 = fields.next().and_then(|f| f.parse().ok()).unwrap_or(1);
    if fields.next().is_some_and(|z| z != "0") {
        return 1;
    }
    count.max(1)
}

/// Capture transport for a split window: every pane laid back out on the window
/// grid, plus pane 0's cursor.
///
/// The cursor rides along because this path also serves live-send whenever no VT
/// channel is available (arming failed, `[tmux] vt_live` off, non-unix).
/// Dropping it there would cost a split preview its painted cursor and, worse,
/// the alternate-screen and mouse-mode flags the wheel forward reads to decide
/// how to scroll a full-screen agent.
fn capture_composited(
    name: &str,
    lines: usize,
    forward_empty: bool,
    deadline: &crate::tmux::TmuxCommandDeadline,
) -> (Option<String>, Option<crate::tmux::PaneCursor>) {
    let session = crate::tmux::Session::from_name(name);
    match session.capture_window_composited_with_cursor_with_deadline(lines, deadline) {
        Ok((content, cursor)) => (Some(content), cursor),
        Err(_) if forward_empty => (Some(String::new()), None),
        Err(_) => (None, None),
    }
}
/// How long a cached window layout serves the composite before the panes
/// around pane 0 are re-captured.
///
/// Only pane 0 receives input, so only its latency can be felt; the panes
/// beside it are being watched, not driven. Refreshing them at this cadence
/// while pane 0 comes from its VT grid every frame keeps echo latency
/// identical to an unsplit session, at roughly three forks a second instead of
/// one per frame.
const COMPOSITE_LAYOUT_MS: u64 = 300;

/// Composite transport for a split window with a live VT channel on pane 0.
///
/// Pane 0's rows come from the grid every call (so typing echoes at full
/// speed); every other pane comes from `cache`, re-captured on the
/// [`COMPOSITE_LAYOUT_MS`] cadence. Falls back to the all-panes fork whenever
/// there is no usable layout or the grid cannot be read.
///
/// `last_pane_probe` is reset whenever the layout capture fails, which is the
/// signal that `pane_count` has gone stale: the chained capture addresses
/// `^.0..^.{pane_count-1}`, so a pane the user closed since the last probe makes
/// tmux exit non-zero (`can't find pane: N`) for the whole invocation. Without
/// that reset the count would stay wrong until [`PANE_COUNT_PROBE_MS`] elapsed,
/// and because the cache is only stamped on success the failing fork would be
/// retried every frame while the preview kept compositing a ghost of the closed
/// pane from the stale rectangles.
struct CompositeCaptureState<'a> {
    layout: &'a mut Option<(std::time::Instant, crate::tmux::composite::WindowLayout)>,
    last_pane_probe: &'a mut Option<std::time::Instant>,
}
fn capture_composited_over_grid(
    name: &str,
    channel: &crate::tmux::vt::VtChannel,
    state: CompositeCaptureState<'_>,
    pane_count: u16,
    lines: usize,
    forward_empty: bool,
    deadline: &crate::tmux::TmuxCommandDeadline,
) -> (Option<String>, Option<crate::tmux::PaneCursor>) {
    let stale = state.layout.as_ref().is_none_or(|(at, _)| {
        at.elapsed() >= std::time::Duration::from_millis(COMPOSITE_LAYOUT_MS)
    });
    if stale {
        match crate::tmux::Session::from_name(name)
            .capture_window_layout_with_deadline(pane_count, deadline)
        {
            Some(layout) => *state.layout = Some((std::time::Instant::now(), layout)),
            // The window is no longer the one these rectangles describe. Drop
            // them rather than compositing a pane that is gone, force a count
            // re-probe on the next cycle, and let the fallback below carry this
            // frame: it probes `window_panes` in the same fork as its capture,
            // so it is correct no matter how stale the count had become.
            None => {
                *state.layout = None;
                *state.last_pane_probe = None;
            }
        }
    }

    let Some((_, layout)) = state.layout.as_ref() else {
        return capture_composited(name, lines, forward_empty, deadline);
    };
    let Some(first) = layout.first_pane() else {
        return capture_composited(name, lines, forward_empty, deadline);
    };
    let Some((rows, mut cursor)) =
        channel.sample_rows_padded_with_deadline(first.width, first.height, deadline)
    else {
        return capture_composited(name, lines, forward_empty, deadline);
    };

    // The sampled cursor stays pane relative after its rows are painted on
    // the window grid. Rebase only the frame dimensions and carry pane 0's
    // rectangle so the renderer can add its origin.
    cursor.pane_height = layout.window_height;
    cursor.pane_width = layout.window_width;
    // A composite carries no scrollback (panes have independent histories), so
    // the preview must not advertise any to scroll into.
    cursor.history_size = 0;
    cursor.composite_pane0 = Some(first);
    (
        Some(layout.composite_with_first_pane_rows(&rows)),
        Some(cursor),
    )
}

/// The default capture transport: one `capture-pane` fork that folds in the
/// cursor probe. Shared by the worker's non-VT path on all platforms.
fn capture_via_tmux(
    name: &str,
    lines: usize,
    forward_empty: bool,
    deadline: &crate::tmux::TmuxCommandDeadline,
) -> (Option<String>, Option<crate::tmux::PaneCursor>) {
    let session = crate::tmux::Session::from_name(name);
    match session.capture_pane_with_cursor_with_deadline(lines, deadline) {
        Ok((content, cur)) => (Some(content), cur),
        Err(_) if forward_empty => (Some(String::new()), None),
        Err(_) => (None, None),
    }
}

#[cfg(unix)]
fn shutdown_vt_source(
    source: &mut Option<std::sync::Arc<crate::tmux::vt::VtChannel>>,
    deadline: &crate::tmux::TmuxCommandDeadline,
) {
    let Some(source) = source.take() else {
        return;
    };
    if let Ok(channel) = std::sync::Arc::try_unwrap(source) {
        channel.shutdown_with_deadline(deadline);
    }
}

#[cfg(unix)]
fn shutdown_osc52_source(
    source: &mut Option<std::sync::Arc<crate::tmux::vt::Osc52Channel>>,
    deadline: &crate::tmux::TmuxCommandDeadline,
) {
    let Some(source) = source.take() else {
        return;
    };
    if let Ok(channel) = std::sync::Arc::try_unwrap(source) {
        channel.shutdown_with_deadline(deadline);
    }
}
impl LiveCaptureWorker {
    pub(in crate::tui) fn spawn(wake: std::sync::Arc<tokio::sync::Notify>) -> Self {
        use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
        use std::sync::{Arc, Condvar, Mutex};
        let capture_lines = Arc::new(AtomicUsize::new(0));
        let target: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let latest: Arc<Mutex<Option<CaptureFrame>>> = Arc::new(Mutex::new(None));
        let interval_ms = Arc::new(AtomicU64::new(LIVE_CAPTURE_INTERVAL_IDLE_MS));
        let live = Arc::new(AtomicBool::new(false));
        let forward_empty = Arc::new(AtomicBool::new(false));
        let nudge: CaptureWake = Arc::new((Mutex::new(0), Condvar::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let clipboard: Arc<Mutex<Option<ClipboardFrame>>> = Arc::new(Mutex::new(None));
        let generation = Arc::new(AtomicU64::new(0));
        // Spawned enabled; the render reconcile pushes the real
        // `[tmux] vt_live` value right after spawn (and on every config
        // refresh), so the worker itself never touches the config file.
        let vt_enabled = Arc::new(AtomicBool::new(true));
        // Start disabled until the render reconcile publishes the Clipboard
        // Pass-through setting, avoiding an observer before configuration is
        // available for a newly created worker.
        let clipboard_capture_enabled = Arc::new(AtomicBool::new(false));
        let cycles = Arc::new(AtomicU64::new(0));
        let cycles_cell = cycles.clone();
        let lines_cell = capture_lines.clone();
        let target_cell = target.clone();
        let slot = latest.clone();
        let interval_cell = interval_ms.clone();
        let live_cell = live.clone();
        let forward_empty_cell = forward_empty.clone();
        let nudge_thread = nudge.clone();
        let stop_flag = stop.clone();
        let clipboard_cell = clipboard.clone();
        let generation_cell = generation.clone();
        #[cfg(unix)]
        let vt_enabled_cell = vt_enabled.clone();
        #[cfg(unix)]
        let clipboard_capture_enabled_cell = clipboard_capture_enabled.clone();
        let thread = std::thread::spawn(move || {
            let mut last_target = String::new();
            let mut last_generation = 0;
            let mut observed_wake = 0;

            #[cfg(unix)]
            let mut next_authoritative_capture: Option<std::time::Instant> = None;
            let mut last_captured: Option<String> = None;
            // Budget the currently-held capture was published at. A budget
            // change alone (scroll depth, viewport resize over a quiet pane)
            // must republish even when the bytes are identical, or consumers
            // waiting for a wider/deeper capture stall forever.
            let mut last_published_budget: usize = 0;
            let mut last_published_cursor: Option<crate::tmux::PaneCursor> = None;
            // When the frame that is currently in the mailbox (or the last
            // one consumed) was published, for the inter-publish floor.
            let mut last_published_at: Option<std::time::Instant> = None;
            // When the current unpublished change first started being held by
            // the repaint-quiescence debounce, for its latency cap. `None`
            // whenever nothing is pending. Cleared on publish, on retarget, and
            // whenever a cycle ends with nothing deferred.
            let mut pending_since: Option<std::time::Instant> = None;
            // Render the live preview from an in-process vt100 grid fed by
            // `tmux pipe-pane -IO` (and route input back through the same
            // socket), instead of scraping `capture-pane` and forking
            // `send-keys` per keystroke. This is the default; the
            // `[tmux] vt_live` setting turns it off (`vt_enabled_cell`,
            // re-read every cycle so a settings change applies in place).
            // When a pane can't arm a VT channel (tmux < 3.4, stopped pane), the
            // worker also falls back to capture for that pane.
            #[cfg(unix)]
            let mut vt_source: Option<std::sync::Arc<crate::tmux::vt::VtChannel>> = None;
            // Terminal snapshots do not include raw OSC 52 escapes. When VT is
            // intentionally off for a terminal pane, this observer restores
            // clipboard forwarding without constructing a grid or seed.
            #[cfg(unix)]
            let mut osc52_source: Option<
                std::sync::Arc<crate::tmux::vt::Osc52Channel>,
            > = None;
            #[cfg(unix)]
            let mut osc52_seen = 0;
            // When the last arm attempt for the current target ran, so a
            // failure (or a channel death: pane killed and the tmux session
            // recreated under the same name, e.g. a session restart) retries
            // on a throttle instead of either re-arming every 25ms tick or
            // latching onto the capture fallback until the user switches
            // panes. Reset on target change so a new pane arms immediately.
            #[cfg(unix)]
            let mut last_vt_arm: Option<std::time::Instant> = None;
            #[cfg(unix)]
            let mut last_osc52_arm: Option<std::time::Instant> = None;
            // Panes in the target window, refreshed on the lazy
            // `PANE_COUNT_PROBE_MS` cadence. The seed only covers the window
            // between arming and the first probe, which runs on the first cycle
            // (`last_pane_probe` starts unset), so an already-split window
            // composites immediately rather than showing one pane first. Reset on
            // retarget so a probe runs for the new pane.
            let mut pane_count: u16 = 1;
            let mut last_pane_probe: Option<std::time::Instant> = None;
            // Window geometry plus the captured rows of every pane, reused
            // across frames while pane 0 is re-rendered from its VT grid.
            // Dropped on retarget and whenever the pane count changes, since
            // the cached rectangles then describe a layout that is gone.
            let mut composite_layout: Option<(
                std::time::Instant,
                crate::tmux::composite::WindowLayout,
            )> = None;
            while !stop_flag.load(Ordering::Relaxed) {
                cycles_cell.fetch_add(1, Ordering::Relaxed);
                let lines = lines_cell.load(Ordering::Relaxed);
                // Read the target without holding the lock across the fork:
                // `set_target` must never wait on a `capture-pane`.
                let name = target_cell
                    .lock()
                    .ok()
                    .map(|g| g.clone())
                    .unwrap_or_default();
                // How long a change deferred this iteration must wait before
                // re-checking (the sooner of the publish-floor and debounce
                // remainders). `Some` shrinks the wait below so the held frame
                // goes out as soon as its blockers reopen; `None` means nothing
                // was deferred.
                let mut defer_wait_ms: Option<u64> = None;
                // The generation this cycle's frames belong to. Read once per
                // cycle so a mid-fork retarget is caught by the still_current
                // recheck below, and a frame that slips past it still carries
                // the old generation for the consumer to drop.
                let generation_now = generation_cell.load(Ordering::Relaxed);
                let command_deadline = crate::tmux::TmuxCommandDeadline::new();
                // A retarget resets the dedup so the new generation's first
                // frame always publishes, even in an ABA switch A -> B -> A
                // that happens entirely between worker cycles and leaves the
                // name unchanged.
                if capture_target_changed(&last_target, last_generation, &name, generation_now) {
                    last_target = name.clone();
                    last_generation = generation_now;
                    last_captured = None;
                    last_published_budget = 0;
                    last_published_cursor = None;

                    // The new pane's first frame must not inherit the old
                    // pane's floor or debounce hold.
                    last_published_at = None;
                    pending_since = None;
                    // Tear down a VT source armed for the old target (also fires
                    // on retarget-to-empty), disabling its `pipe-pane`, and allow
                    // a fresh arm attempt for the new target.
                    #[cfg(unix)]
                    {
                        shutdown_vt_source(&mut vt_source, &command_deadline);
                        last_vt_arm = None;
                        next_authoritative_capture = None;
                        shutdown_osc52_source(&mut osc52_source, &command_deadline);
                        osc52_seen = 0;
                        last_osc52_arm = None;
                    }
                    pane_count = 1;
                    last_pane_probe = None;
                    composite_layout = None;
                }
                // `[tmux] vt_live`, re-read every cycle. Toggling off while a
                // channel is armed tears it down (disabling its `pipe-pane`);
                // resetting the arm latch lets a later re-enable arm afresh
                // for the same target instead of waiting for a retarget.
                #[cfg(unix)]
                let vt_enabled = vt_enabled_cell.load(Ordering::Relaxed);
                #[cfg(unix)]
                let clipboard_capture_enabled =
                    clipboard_capture_enabled_cell.load(Ordering::Relaxed);
                // The throttle resets even when no channel is armed (a failed
                // arm attempt), so re-enabling always arms on the next cycle.
                #[cfg(unix)]
                if !vt_enabled {
                    shutdown_vt_source(&mut vt_source, &command_deadline);
                    last_vt_arm = None;
                    next_authoritative_capture = None;
                }
                #[cfg(unix)]
                if !clipboard_capture_enabled {
                    shutdown_osc52_source(&mut osc52_source, &command_deadline);
                    osc52_seen = 0;
                    last_osc52_arm = None;
                }
                if lines > 0 && !name.is_empty() {
                    let forward_empty_policy = forward_empty_cell.load(Ordering::Relaxed);
                    // Keep a lazy count of the target window's panes so a
                    // hand-made split stops being invisible. One tiny fork
                    // every couple of seconds on the single previewed pane.
                    if last_pane_probe.is_none_or(|t| {
                        t.elapsed() >= std::time::Duration::from_millis(PANE_COUNT_PROBE_MS)
                    }) {
                        last_pane_probe = Some(std::time::Instant::now());
                        let seen = probe_pane_count(&name, &command_deadline);
                        if seen != pane_count {
                            // Layout changed under us; the cached rectangles no
                            // longer describe this window.
                            composite_layout = None;
                            pane_count = seen;
                        }
                    }
                    // A split window renders through the compositor in BOTH
                    // passive and live mode. With a VT channel armed the split
                    // costs no more per frame than an unsplit session: pane 0
                    // still comes from the grid, and only the panes beside it
                    // are re-captured, on their own slower cadence.
                    let composite = pane_count > 1;
                    // An OSC 52 clipboard write the displayed pane emitted
                    // since the last cycle. Published below under the same
                    // retarget guard as the cursor.
                    #[cfg(unix)]
                    let observe_osc52 =
                        !vt_enabled && forward_empty_policy && clipboard_capture_enabled;
                    #[cfg(unix)]
                    if !observe_osc52 {
                        shutdown_osc52_source(&mut osc52_source, &command_deadline);
                        last_osc52_arm = None;
                    }
                    #[cfg(unix)]
                    if observe_osc52
                        && osc52_source.is_none()
                        && last_osc52_arm.is_none_or(|t| t.elapsed() >= VT_REARM_INTERVAL)
                    {
                        last_osc52_arm = Some(std::time::Instant::now());
                        osc52_source = crate::tmux::vt::Osc52Channel::acquire_with_deadline(
                            &name,
                            &command_deadline,
                        );
                        if let Some(source) = osc52_source.as_ref() {
                            osc52_seen = source.clipboard_sequence();
                        }
                    }
                    #[cfg(unix)]
                    if osc52_source
                        .as_ref()
                        .is_some_and(|source| !source.is_alive())
                    {
                        shutdown_osc52_source(&mut osc52_source, &command_deadline);
                        osc52_seen = 0;
                    }
                    #[cfg(unix)]
                    let mut clipboard_now = osc52_source.as_ref().and_then(|source| {
                        source.refresh_owner_heartbeat_with_deadline(&command_deadline);
                        source.clipboard_after(&mut osc52_seen)
                    });
                    #[cfg(not(unix))]
                    let clipboard_now: Option<String> = None;
                    // Acquire one frame + cursor. Default: sample the in-process
                    // vt100 grid, arming a `pipe-pane -IO` channel on first use
                    // for this target (cursor and alt/mouse flags come
                    // authoritatively from the grid). If arming fails (tmux too
                    // old, stopped pane), fall back to a `capture-pane` fork for
                    // this pane and retry on the `VT_REARM_INTERVAL` throttle.
                    // Capture-path policy: outside live-send, empty captures
                    // are FORWARDED (a missing or killed pane must surface
                    // as "No output available", not stale bytes); during
                    // live-send the #1501 kill switch preserves the
                    // last-good frame against transient tmux errors.
                    let forward_empty = forward_empty_policy || !live_cell.load(Ordering::Relaxed);
                    #[cfg(unix)]
                    let (capture, cursor_now) = if vt_enabled {
                        if vt_source.is_none()
                            && last_vt_arm.is_none_or(|t| t.elapsed() >= VT_REARM_INTERVAL)
                        {
                            last_vt_arm = Some(std::time::Instant::now());
                            vt_source = crate::tmux::vt::VtChannel::acquire_with_deadline(
                                &name,
                                &command_deadline,
                            );
                            // Event-driven echo: the channel pokes our nudge
                            // condvar on every grid change, so the wait below
                            // ends the moment output lands instead of after
                            // the remainder of a poll interval.
                            if let Some(v) = vt_source.as_ref() {
                                v.set_change_wakeup(nudge_thread.clone());
                                next_authoritative_capture = Some(
                                    std::time::Instant::now() + AUTHORITATIVE_CAPTURE_INTERVAL,
                                );
                            }
                        }
                        // A channel whose forwarder has disconnected stops
                        // updating its grid; drop it and fall back to capture
                        // for this pane. The arm throttle above then re-arms
                        // after `VT_REARM_INTERVAL` (a session restart reuses
                        // the tmux name, so the pane usually comes back)
                        // without thrashing on a permanently broken pane.
                        if vt_source.as_ref().is_some_and(|v| !v.is_alive()) {
                            shutdown_vt_source(&mut vt_source, &command_deadline);
                            next_authoritative_capture = None;
                        }
                        let authoritative_due = vt_source.is_some()
                            && authoritative_capture_due(
                                next_authoritative_capture,
                                std::time::Instant::now(),
                            );
                        let refresh_result = authoritative_due.then(|| match vt_source.as_ref() {
                            Some(source)
                                if !authoritative_refresh_is_quiet(source.chunk_timing()) =>
                            {
                                crate::tmux::vt::VtRefreshResult::Busy
                            }
                            Some(source) => source.refresh_authoritatively(&command_deadline),
                            None => crate::tmux::vt::VtRefreshResult::Failed,
                        });
                        match authoritative_refresh_action(authoritative_due, refresh_result) {
                            AuthoritativeRefreshAction::None => {}
                            AuthoritativeRefreshAction::Schedule(after) => {
                                next_authoritative_capture =
                                    Some(std::time::Instant::now() + after);
                            }
                        }
                        match vt_source.as_ref() {
                            Some(v) => {
                                clipboard_now = v.take_clipboard();
                                if composite {
                                    capture_composited_over_grid(
                                        &name,
                                        v,
                                        CompositeCaptureState {
                                            layout: &mut composite_layout,
                                            last_pane_probe: &mut last_pane_probe,
                                        },
                                        pane_count,
                                        lines,
                                        forward_empty,
                                        &command_deadline,
                                    )
                                } else {
                                    let (content, cur) =
                                        v.sample_with_deadline(lines, &command_deadline);
                                    (Some(content), cur)
                                }
                            }
                            // No grid for pane 0, so every pane comes from the
                            // fork instead.
                            None if composite => {
                                capture_composited(&name, lines, forward_empty, &command_deadline)
                            }
                            None => {
                                capture_via_tmux(&name, lines, forward_empty, &command_deadline)
                            }
                        }
                    } else if composite {
                        capture_composited(&name, lines, forward_empty, &command_deadline)
                    } else {
                        capture_via_tmux(&name, lines, forward_empty, &command_deadline)
                    };
                    #[cfg(not(unix))]
                    let (capture, cursor_now) = if composite {
                        capture_composited(&name, lines, forward_empty, &command_deadline)
                    } else {
                        capture_via_tmux(&name, lines, forward_empty, &command_deadline)
                    };
                    // Chunk-arrival timing for the repaint-quiescence debounce,
                    // only when sampling a live VT grid. `None` on the
                    // capture-pane fallback and non-unix, which leaves pacing to
                    // the publish floor alone.
                    #[cfg(unix)]
                    let vt_timing = vt_source.as_ref().and_then(|v| v.chunk_timing());
                    #[cfg(not(unix))]
                    let vt_timing: Option<(u64, u64)> = None;
                    if let Some(content) = capture {
                        // Skip unchanged frames (no point waking a re-parse).
                        // Empties are skipped too unless this pane forwards
                        // them; only changed captures reach (and wake) the
                        // render loop, so an idle pane never repaints.
                        let changed = last_captured.as_deref() != Some(content.as_str());

                        // Recheck the target once for both publishes: a retarget
                        // mid-fork means these bytes (and this cursor) belong to
                        // the old pane and must not land under the new view.
                        // `set_target` also clears the mailbox, but the fork may
                        // have started before that switch.
                        let still_current = target_cell.lock().ok().is_some_and(|g| *g == name)
                            && generation_cell.load(Ordering::Relaxed) == generation_now;
                        if still_current {
                            // Publish an agent clipboard write and wake the
                            // render loop even if the frame itself dedups: a
                            // copy must reach the host clipboard promptly
                            // whether or not the grid changed visibly.
                            if let Some(text) = clipboard_now {
                                if let Ok(mut guard) = clipboard_cell.lock() {
                                    *guard = Some(ClipboardFrame {
                                        generation: generation_now,
                                        target: name.clone(),
                                        text,
                                    });
                                }
                                wake.notify_one();
                            }
                            // A budget change alone (deeper scroll, viewport
                            // resize over a quiet pane) must republish even
                            // when the bytes are identical, or consumers
                            // waiting for a wider/deeper capture stall.
                            if (forward_empty || !content.is_empty())
                                && frame_needs_publish(
                                    changed,
                                    last_published_cursor,
                                    cursor_now,
                                    lines != last_published_budget,
                                )
                            {
                                let since =
                                    last_published_at.map(|t| t.elapsed().as_millis() as u64);
                                let floor = publish_floor_wait_ms(since);
                                // Repaint-quiescence debounce (VT path only):
                                // hold a changed frame while output streams in
                                // back-to-back chunks so a multi-chunk
                                // clear-then-reprint publishes once it settles,
                                // not mid-repaint (#2903). A lone chunk (a
                                // keystroke echo) reports not-streaming and
                                // never waits, preserving the #2822 echo path.
                                let debounce = match vt_timing {
                                    Some((since_last_chunk, gap)) => {
                                        let streaming = gap < SAMPLE_QUIESCENCE_MS;
                                        let held = pending_since
                                            .map(|t| t.elapsed().as_millis() as u64)
                                            .unwrap_or(0);
                                        sample_debounce_wait_ms(streaming, since_last_chunk, held)
                                    }
                                    None => 0,
                                };
                                if floor == 0 && debounce == 0 {
                                    if let Ok(mut guard) = slot.lock() {
                                        *guard = Some(CaptureFrame {
                                            generation: generation_now,
                                            target: name.clone(),
                                            budget: lines,
                                            content: content.clone(),
                                            cursor: cursor_now,
                                        });
                                    }
                                    last_captured = Some(content);
                                    last_published_budget = lines;
                                    last_published_cursor = cursor_now;
                                    last_published_at = Some(std::time::Instant::now());
                                    pending_since = None;
                                    wake.notify_one();
                                } else {
                                    // Held by the floor and/or the debounce:
                                    // defer, don't drop. `last_captured` stays
                                    // stale so the next cycle re-detects this
                                    // frame and publishes it once both reopen.
                                    if pending_since.is_none() {
                                        pending_since = Some(std::time::Instant::now());
                                    }
                                    let wait = match (floor, debounce) {
                                        (0, d) => d,
                                        (f, 0) => f,
                                        (f, d) => f.min(d),
                                    };
                                    defer_wait_ms = Some(wait.max(1));
                                }
                            }
                        }
                    }
                }
                // Nothing deferred this cycle means no frame is being held, so
                // clear the debounce hold and let the next repaint's latency cap
                // start fresh. Covers both a publish (already cleared above) and
                // a mid-repaint change that evaporated back to the last
                // published content without ever publishing, which would
                // otherwise leave a stale `pending_since` that makes the next
                // repaint hit the cap immediately and skip the debounce.
                if defer_wait_ms.is_none() {
                    pending_since = None;
                }
                // Interruptible wait: `set_live` / `set_target` notify the
                // condvar so a cadence or target change is picked up at once
                // rather than after the current sleep, and on the VT path the
                // channel's reader thread notifies it on every grid change,
                // so fresh output samples immediately instead of waiting out
                // the interval. A generation under the condvar mutex preserves
                // a wake that arrives before this thread parks; multiple wakes
                // may coalesce into one extra cycle, which dedup makes harmless.
                //
                // A live in-process vt channel samples the grid cheaply (no
                // `capture-pane` fork) and dedups unchanged frames, so the idle
                // throttle buys nothing there: pace it fast so the PREVIEWED
                // pane scrolls / streams as smoothly as the active live pane,
                // even when it isn't the live-send target. The idle cadence
                // still governs the capture-pane fallback, whose every sample is
                // an expensive fork.
                #[cfg(unix)]
                let vt_active = vt_source.as_ref().is_some_and(|v| v.is_alive());
                #[cfg(not(unix))]
                let vt_active = false;
                let ms = if vt_active {
                    LIVE_CAPTURE_INTERVAL_FAST_MS
                } else {
                    interval_cell.load(Ordering::Relaxed)
                };
                // A deferred frame goes out as soon as its blockers (publish
                // floor and/or debounce) reopen, not after a full interval.
                let ms = match defer_wait_ms {
                    Some(wait) => ms.min(wait),
                    None => ms,
                };
                let _ = wait_for_capture_wake(
                    &nudge_thread,
                    &mut observed_wake,
                    std::time::Duration::from_millis(ms),
                );
            }
            #[cfg(unix)]
            {
                let deadline = crate::tmux::TmuxCommandDeadline::new();
                shutdown_vt_source(&mut vt_source, &deadline);
                shutdown_osc52_source(&mut osc52_source, &deadline);
            }
        });
        #[cfg(not(test))]
        drop(thread);
        Self {
            capture_lines,
            target,
            latest,
            generation,
            interval_ms,
            live,
            forward_empty,
            nudge,
            stop,
            clipboard,
            vt_enabled,
            clipboard_capture_enabled,
            cycles,
            #[cfg(test)]
            test_id: LIVE_CAPTURE_WORKER_TEST_COUNTER
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            #[cfg(test)]
            thread: Some(thread),
        }
    }

    /// Push the `[tmux] vt_live` setting into the worker. Cheap (one atomic
    /// store); called right after spawn and on every config refresh, so a
    /// toggle applies on the next capture cycle without a respawn. The nudge
    /// makes that cycle run now (a disable mid-idle-sleep would otherwise
    /// keep the armed channel for up to 250ms).
    pub(in crate::tui) fn set_vt_enabled(&self, enabled: bool) {
        let prev = self
            .vt_enabled
            .swap(enabled, std::sync::atomic::Ordering::Relaxed);
        if prev != enabled {
            self.nudge();
        }
    }

    /// Enable raw OSC 52 observation for terminal previews that render via
    /// capture-pane. This does not affect a VT grid, which already extracts
    /// clipboard writes from its own pipe.
    pub(in crate::tui) fn set_clipboard_capture_enabled(&self, enabled: bool) {
        let prev = self
            .clipboard_capture_enabled
            .swap(enabled, std::sync::atomic::Ordering::Relaxed);
        if prev != enabled {
            self.nudge();
        }
    }

    /// A cloneable handle the send worker uses to nudge this worker after
    /// each dispatched keystroke batch (echo latency). Backed by the same
    /// condvar `set_live` / `set_target` use, so a wake just runs one
    /// capture cycle early.
    pub(in crate::tui) fn waker(&self) -> LiveCaptureWake {
        LiveCaptureWake {
            nudge: self.nudge.clone(),
        }
    }

    /// Choose whether empty captures clear the preview (terminal / container
    /// panes) or preserve the last-good frame (agent / tool panes, the #1501
    /// kill switch). Cheap (one atomic store); called from the render
    /// reconcile alongside `set_target` so the policy tracks the displayed
    /// pane type without a respawn.
    pub(in crate::tui) fn set_forward_empty(&self, forward: bool) {
        self.forward_empty
            .store(forward, std::sync::atomic::Ordering::Relaxed);
    }

    /// Current empty-frame policy, re-read by the render consumer after a
    /// potentially blocking capture. Terminal/container panes always clear;
    /// agent/tool panes clear only outside live-send, preserving #1501 across
    /// a live-mode transition that races the capture.
    pub(in crate::tui) fn should_forward_empty(&self) -> bool {
        self.forward_empty
            .load(std::sync::atomic::Ordering::Relaxed)
            || !self.live.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Wake the worker out of its inter-capture wait so a just-changed
    /// cadence or target applies immediately.
    fn nudge(&self) {
        signal_capture_wake(&self.nudge);
    }

    /// Point the worker at a different pane (its tmux session name; empty to
    /// idle). Cheap: just swaps the shared name and drops any capture queued
    /// from the previous pane so the render never applies stale bytes under
    /// the new view. Never blocks on a `capture-pane` fork, so the render
    /// reconcile can call it freely.
    pub(in crate::tui) fn set_target(&self, name: String) {
        let changed = if let Ok(mut guard) = self.target.lock() {
            if *guard != name {
                *guard = name;
                // Invalidate every frame the in-flight cycle might publish:
                // it tagged its frames with the old generation, so even a
                // frame that slips past the mailbox clear is dropped by
                // `frame_is_current`.
                self.generation
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if let Ok(mut latest) = self.latest.lock() {
                    *latest = None;
                }
                // pane must not land on the host clipboard under the new
                // view.
                if let Ok(mut clipboard) = self.clipboard.lock() {
                    *clipboard = None;
                }
                true
            } else {
                false
            }
        } else {
            false
        };
        if changed {
            // Capture the new pane now instead of after the current sleep.
            self.nudge();
        }
    }

    /// Switch the capture cadence between live-send (fast) and background
    /// preview (idle). Cheap (one atomic store); called from the render
    /// reconcile so entering/leaving live mode retunes the worker in place
    /// without a respawn. Does NOT touch the cursor: it is published every
    /// cycle now (the passive wheel forward reads it), and the render only
    /// paints it under live-send, so a backgrounded preview never shows one.
    pub(in crate::tui) fn set_live(&self, live: bool) {
        let ms = if live {
            LIVE_CAPTURE_INTERVAL_FAST_MS
        } else {
            LIVE_CAPTURE_INTERVAL_IDLE_MS
        };
        let prev = self
            .interval_ms
            .swap(ms, std::sync::atomic::Ordering::Relaxed);
        self.live.store(live, std::sync::atomic::Ordering::Relaxed);
        if prev != ms {
            // Apply the new cadence now: a mid-idle-sleep worker would
            // otherwise keep the old (up to 250ms) interval for one cycle,
            // lagging the first live capture on live-send entry.
            self.nudge();
        }
    }

    /// Publish the line count the worker should capture. Cheap (one atomic
    /// store); called each render so resizes and history scroll reach the
    /// worker promptly.
    pub(in crate::tui) fn set_capture_lines(&self, lines: usize) {
        self.capture_lines
            .store(lines, std::sync::atomic::Ordering::Relaxed);
    }

    /// Put a frame a consumer rejected back into the mailbox so it survives
    /// until one can apply it. The worker's dedup is content-based, so a
    /// consumed-and-dropped frame (e.g. a terminal-clear empty frame landing
    /// while the preview is frozen) would otherwise never be republished.
    /// Never overwrites a newer frame.
    pub(in crate::tui) fn restore_latest(&self, frame: CaptureFrame) {
        if let Ok(mut guard) = self.latest.lock() {
            if guard.is_none() {
                *guard = Some(frame);
            }
        }
    }

    /// Take the newest frame the worker has produced since the last call,
    /// if any. Returns `None` when nothing new has arrived (the render loop
    /// then keeps the current preview).
    pub(in crate::tui) fn take_latest(&self) -> Option<CaptureFrame> {
        self.latest.lock().ok().and_then(|mut guard| guard.take())
    }

    /// Snapshot of the worker's cycle counter for render-side stall detection.
    /// Publication is deliberately not used because unchanged panes publish
    /// nothing while a healthy worker continues sampling.
    pub(in crate::tui) fn cycles(&self) -> u64 {
        self.cycles.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Whether `frame` belongs to the worker's CURRENT target generation.
    /// A frame captured before the last set_target must be dropped, never
    /// applied or restored under the new target.
    pub(in crate::tui) fn frame_is_current(&self, frame: &CaptureFrame) -> bool {
        self.capture_identity_is_current(&frame.target, frame.generation)
    }

    pub(in crate::tui) fn capture_identity_is_current(
        &self,
        target: &str,
        generation: u64,
    ) -> bool {
        self.generation.load(std::sync::atomic::Ordering::Relaxed) == generation
            && self
                .target
                .lock()
                .ok()
                .is_some_and(|current| *current == target)
    }
    #[cfg(test)]
    pub(in crate::tui) fn inject_stale_generation_frame_for_test(
        &self,
        budget: usize,
        content: &str,
    ) {
        let generation = self.current_generation_for_test();
        if let Ok(mut latest) = self.latest.lock() {
            *latest = Some(CaptureFrame {
                generation: generation.wrapping_sub(1),
                target: self
                    .target
                    .lock()
                    .ok()
                    .map(|value| value.clone())
                    .unwrap_or_default(),
                budget,
                content: content.to_string(),
                cursor: None,
            });
        }
    }

    /// Take the newest OSC 52 clipboard write the displayed pane's agent has
    /// emitted since the last call, if any.
    pub(in crate::tui) fn take_agent_clipboard(&self) -> Option<String> {
        let frame = self.clipboard.lock().ok()?.take()?;
        self.capture_identity_is_current(&frame.target, frame.generation)
            .then_some(frame.text)
    }

    /// Current target generation for tests that hand-build frames.
    #[cfg(test)]
    pub(in crate::tui) fn current_generation_for_test(&self) -> u64 {
        self.generation.load(std::sync::atomic::Ordering::Relaxed)
    }
    #[cfg(test)]
    pub(in crate::tui) fn stop_for_test(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        self.nudge();
        if let Some(thread) = self.thread.take() {
            thread.join().expect("capture worker must stop cleanly");
        }
    }

    #[cfg(test)]
    pub(in crate::tui) fn id_for_test(&self) -> u64 {
        self.test_id
    }
    #[cfg(test)]
    pub(in crate::tui) fn set_cycles_for_test(&self, cycles: u64) {
        self.cycles
            .store(cycles, std::sync::atomic::Ordering::Relaxed);
    }

    /// Hand-publish a frame into the mailbox for consumer-side tests.
    #[cfg(test)]
    pub(in crate::tui) fn inject_frame_for_test(&self, budget: usize, content: &str) {
        self.inject_frame_with_cursor_for_test(budget, content, None);
    }

    #[cfg(test)]
    pub(in crate::tui) fn inject_frame_with_cursor_for_test(
        &self,
        budget: usize,
        content: &str,
        cursor: Option<crate::tmux::PaneCursor>,
    ) {
        if let Ok(mut latest) = self.latest.lock() {
            *latest = Some(CaptureFrame {
                generation: self.current_generation_for_test(),
                target: self
                    .target
                    .lock()
                    .ok()
                    .map(|value| value.clone())
                    .unwrap_or_default(),
                budget,
                content: content.to_string(),
                cursor,
            });
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResizeDispatchResult {
    None,
    Succeeded,
    Failed,
}

/// Walk one drained batch and execute it as one-shot tmux subprocesses.
/// Coalescing merges literal-key runs into a single send-keys call; named
/// keys and resizes dispatch individually.
fn dispatch_batch(
    tmux_name: &str,
    resize_owner: &str,
    batch: Vec<WorkerMsg>,
) -> ResizeDispatchResult {
    let actions = coalesce(batch);
    // A Paste can only go through tmux (paste-buffer -p decides whether the
    // pane gets bracketed-paste markers), so pin the whole mixed batch to tmux
    // and preserve one ordered writer for the pty.
    let force_tmux = actions.iter().any(|a| matches!(a, TmuxAction::Paste(_)));
    let mut resize_result = ResizeDispatchResult::None;
    for action in actions {
        let is_resize = matches!(action, TmuxAction::Resize { .. });
        match dispatch_via_fork(tmux_name, &action, force_tmux, Some(resize_owner)) {
            Ok(()) if is_resize => resize_result = ResizeDispatchResult::Succeeded,
            Ok(()) => {}
            Err(err) => {
                if is_resize {
                    resize_result = ResizeDispatchResult::Failed;
                }
                tracing::warn!(
                    target: "tui.live_send",
                    error = %err,
                    action = ?action,
                    "live-send fork dispatch failed; action dropped",
                );
            }
        }
    }
    resize_result
}

/// Execute one TmuxAction as a one-shot tmux subprocess. Module-level fn
/// (rather than a method on the worker) so it stays callable from the spawned
/// thread without holding a worker reference.
fn dispatch_via_fork(
    tmux_name: &str,
    action: &TmuxAction,
    force_tmux: bool,
    resize_owner: Option<&str>,
) -> anyhow::Result<()> {
    use std::process::Stdio;

    // Fast path (`[tmux] vt_live`): when a *live* input channel is armed for this
    // pane, ALL pane input goes through the socket, never `send-keys`. This is a
    // single-writer invariant: mixing the socket and `send-keys` would interleave
    // two writers on the one pty input stream and can corrupt multi-byte
    // sequences (tmux pipe-pane -I shares the input stream with no arbitration).
    // `input_mode` returns `Some` only while the forwarder is connected, so a
    // not-yet-connected or dead channel reports `None` and input falls through
    // to the `send-keys` fork below instead of vanishing. Keys are encoded to
    // bytes here using the pane's cursor-key mode (DECCKM) from the grid, since
    // we bypass tmux's own key translation. `Resize` is not pane input (it's
    // `resize-window`), so it still forks below.
    //
    // The invariant only forbids *concurrent* writers, not a sequential
    // fallback. `try_send_input`'s `write_all` on a blocking `UnixStream` fails
    // only on a broken pipe / EOF, never a transient WouldBlock, so `false`
    // reliably means the forwarder already died between the `input_mode` check
    // above and this write (a TOCTOU race), not that it's merely busy. At that
    // point the socket has no live writer left, so forking `send-keys` for this
    // one action is safe and delivers the keystroke instead of dropping it
    // silently. An empty-bytes encoding (a key we can't represent while the
    // channel is genuinely alive) still drops without forking: there is no
    // failure to prove the writer is dead, so falling back here could race a
    // still-live socket writer.
    #[cfg(unix)]
    if let Some(app_cursor) = crate::tmux::vt::input_mode(tmux_name).filter(|_| !force_tmux) {
        if !matches!(action, TmuxAction::Resize { .. } | TmuxAction::Paste(_)) {
            let bytes = encode_action_bytes(action, app_cursor);
            if bytes.is_empty() {
                return Ok(());
            }
            if crate::tmux::vt::try_send_input(tmux_name, &bytes) {
                return Ok(());
            }
            tracing::warn!(
                target: "tui.live_send",
                action = ?action,
                "vt socket write failed; falling back to send-keys fork",
            );
            // Fall through to the send-keys fork below.
        }
    }

    let target = format!("{}:^.0", tmux_name);
    let mut cmd = crate::tmux::tmux_command();
    cmd.stderr(Stdio::null());
    match action {
        TmuxAction::Literal(s) => {
            // tmux's command parser treats a trailing `;` in a
            // `send-keys -l` payload as a command separator and silently
            // drops it, even after the `--` end-of-options marker, so a
            // lone or trailing semicolon never reaches the pane (#1942).
            // Peel the trailing semicolons off and deliver them as raw
            // hex bytes (`-H 3b`), which tmux passes through verbatim;
            // the remaining head still rides the literal path. Embedded
            // and leading semicolons survive `-l` fine, so only the
            // trailing run needs the hex detour.
            let (head, semis) = peel_trailing_semicolons(s);
            if semis > 0 {
                if !head.is_empty() {
                    send_literal(&target, head)?;
                }
                return crate::tmux::Session::from_name(tmux_name)
                    .send_raw_bytes(&vec![0x3b; semis]);
            }
            // `-l --` mirrors `send_literal_no_enter`: literal-mode
            // send, followed by the end-of-options marker so a payload
            // starting with `-` isn't reparsed as a flag.
            cmd.args(["send-keys", "-t", &target, "-l", "--", s.as_str()]);
        }
        TmuxAction::Named(name) => {
            cmd.args(["send-keys", "-t", &target, name.as_str()]);
        }
        TmuxAction::NamedRepeat { name, count } => {
            // `-N <count>` repeats the key `count` times in one fork. tmux
            // renders each press in the pane's current cursor-key mode, so
            // the wheel-forward arrows honor DECCKM just like a single
            // `Named` does.
            let count = count.to_string();
            cmd.args(["send-keys", "-t", &target, "-N", &count, name.as_str()]);
        }
        TmuxAction::HexBytes(bytes) => {
            // `-H` sends each subsequent arg as the hex byte value of an
            // ASCII character. We use this for control bytes (CR, TAB,
            // ESC) and the bracketed-paste markers, none of which can
            // ride a `-l` payload safely. Chunking against ARG_MAX and
            // the per-byte hex encoding live in the shared tmux layer
            // (the web live view's input path uses the same fn).
            return crate::tmux::Session::from_name(tmux_name).send_raw_bytes(bytes);
        }
        TmuxAction::Paste(text) => {
            // tmux emits the bracketed-paste markers only if the program in
            // the pane set DECSET 2004, so a raw shell or a SQL REPL gets
            // clean text instead of literal `00~` / `01~` leftovers.
            return crate::tmux::Session::from_name(tmux_name).paste_text(text);
        }
        TmuxAction::Resize { cols, rows } => {
            // Ownership is checked by tmux in the same command queue as the
            // resize. The worker's earlier heartbeat check only filters stale
            // batches; it cannot authorize a later subprocess safely.
            let owner = resize_owner
                .ok_or_else(|| anyhow::anyhow!("live-send resize has no owner token"))?;
            if !crate::tmux::Session::from_name(tmux_name)
                .resize_window_if_owner(owner, *cols, *rows)
            {
                anyhow::bail!("live-send resize lost ownership, failed, or timed out");
            }
            return Ok(());
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

/// Encode a `TmuxAction` to the raw terminal bytes for the persistent-input
/// fast path. We bypass tmux's `send-keys` key translation, so we reproduce it
/// here, honoring the pane's cursor-key mode (`app_cursor`, DECCKM) for arrows
/// and nav keys. Returns an empty vec for a key we can't encode (dropped under
/// the single-writer rule rather than forked). `Resize` never reaches here.
#[cfg(unix)]
fn encode_action_bytes(action: &TmuxAction, app_cursor: bool) -> Vec<u8> {
    match action {
        TmuxAction::Literal(s) => s.clone().into_bytes(),
        // Already raw control bytes (CR/TAB/ESC, bracketed-paste markers).
        TmuxAction::HexBytes(bytes) => bytes.clone(),
        TmuxAction::Named(name) => encode_named_key(name, app_cursor),
        TmuxAction::NamedRepeat { name, count } => {
            let one = encode_named_key(name, app_cursor);
            one.repeat(*count)
        }
        // Paste never reaches here: the vt fast path is skipped for it so
        // tmux can make the bracketed-paste decision.
        TmuxAction::Paste(_) => Vec::new(),
        TmuxAction::Resize { .. } => Vec::new(),
    }
}

/// Strip tmux modifier prefixes (`C-`, `M-`, `S-`, in any order) off a key
/// name, returning `(ctrl, alt, shift, base)`.
#[cfg(unix)]
fn split_mods(name: &str) -> (bool, bool, bool, &str) {
    let (mut ctrl, mut alt, mut shift) = (false, false, false);
    let mut rest = name;
    loop {
        if let Some(r) = rest.strip_prefix("C-") {
            ctrl = true;
            rest = r;
        } else if let Some(r) = rest.strip_prefix("M-") {
            alt = true;
            rest = r;
        } else if let Some(r) = rest.strip_prefix("S-") {
            shift = true;
            rest = r;
        } else {
            break;
        }
    }
    (ctrl, alt, shift, rest)
}

/// Encode one tmux key name (e.g. `Up`, `C-c`, `S-Up`, `M-x`, `F5`) to terminal
/// bytes. Cursor/nav keys honor `app_cursor` (DECCKM) and the xterm modifier
/// parameter (`1 + shift + alt*2 + ctrl*4`). Empty vec = unencodable.
#[cfg(unix)]
fn encode_named_key(name: &str, app_cursor: bool) -> Vec<u8> {
    let (ctrl, alt, shift, base) = split_mods(name);
    let modp = 1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(ctrl);
    let has_mod = modp != 1;
    // ESC-prefix for Alt on the simple (non-CSI) byte forms; CSI forms carry
    // Alt in `modp` instead.
    let meta = |v: Vec<u8>| -> Vec<u8> {
        if alt {
            let mut out = vec![0x1b];
            out.extend_from_slice(&v);
            out
        } else {
            v
        }
    };

    // Cursor + Home/End: SS3 vs CSI by DECCKM when unmodified; CSI 1;modp when
    // modified.
    if let Some(fin) = match base {
        "Up" => Some(b'A'),
        "Down" => Some(b'B'),
        "Right" => Some(b'C'),
        "Left" => Some(b'D'),
        "Home" => Some(b'H'),
        "End" => Some(b'F'),
        _ => None,
    } {
        if has_mod {
            return format!("\x1b[1;{modp}{}", fin as char).into_bytes();
        }
        return if app_cursor {
            vec![0x1b, b'O', fin]
        } else {
            vec![0x1b, b'[', fin]
        };
    }

    // Editing block (CSI n ~), modifier as `;modp`. Not affected by DECCKM.
    // `PageUp`/`PageDown` are accepted alongside the tmux `PPage`/`NPage`
    // names: the wheel- and edge-autoscroll page-forward paths emit the former
    // (tmux `send-keys` takes both), and without the alias those keys would
    // encode to nothing and be dropped on the VT input path.
    if let Some(n) = match base {
        "IC" => Some(2),
        "DC" => Some(3),
        "PPage" | "PageUp" => Some(5),
        "NPage" | "PageDown" => Some(6),
        _ => None,
    } {
        return if has_mod {
            format!("\x1b[{n};{modp}~").into_bytes()
        } else {
            format!("\x1b[{n}~").into_bytes()
        };
    }

    // Function keys: F1-F4 are SS3 P/Q/R/S (CSI 1;modp X when modified),
    // F5-F12 are CSI n ~.
    if let Some(rest) = base.strip_prefix('F') {
        if let Ok(n) = rest.parse::<u8>() {
            if (1..=4).contains(&n) {
                let fin = b'P' + (n - 1);
                return if has_mod {
                    format!("\x1b[1;{modp}{}", fin as char).into_bytes()
                } else {
                    vec![0x1b, b'O', fin]
                };
            }
            let code = match n {
                5 => 15,
                6 => 17,
                7 => 18,
                8 => 19,
                9 => 20,
                10 => 21,
                11 => 23,
                12 => 24,
                _ => return Vec::new(),
            };
            return if has_mod {
                format!("\x1b[{code};{modp}~").into_bytes()
            } else {
                format!("\x1b[{code}~").into_bytes()
            };
        }
    }

    match base {
        "Enter" => meta(vec![b'\r']),
        "Tab" => meta(vec![b'\t']),
        "BTab" => b"\x1b[Z".to_vec(),
        "BSpace" => meta(vec![0x7f]),
        "Escape" => meta(vec![0x1b]),
        "Space" => {
            if ctrl {
                vec![0] // Ctrl-Space -> NUL
            } else {
                meta(vec![b' '])
            }
        }
        _ => {
            // Single char: `C-<letter>` -> C0 control byte; otherwise the char,
            // ESC-prefixed for Alt. (Shift never reaches here: plain chars are
            // sent literally with case already applied.)
            let b = base.as_bytes();
            if b.len() == 1 {
                let c = b[0];
                if ctrl && c.is_ascii_alphabetic() {
                    return meta(vec![c.to_ascii_lowercase() & 0x1f]);
                }
                if c.is_ascii_graphic() {
                    return meta(vec![c]);
                }
            }
            Vec::new()
        }
    }
}

#[cfg(all(test, unix))]
mod vt_input_encode_tests {
    use super::*;

    fn enc(name: &str, app_cursor: bool) -> Vec<u8> {
        encode_named_key(name, app_cursor)
    }

    #[test]
    fn arrows_follow_decckm() {
        assert_eq!(enc("Up", false), b"\x1b[A");
        assert_eq!(enc("Up", true), b"\x1bOA");
        assert_eq!(enc("Down", false), b"\x1b[B");
        assert_eq!(enc("Right", true), b"\x1bOC");
        assert_eq!(enc("Left", false), b"\x1b[D");
        // Home/End are cursor-mode keys too.
        assert_eq!(enc("Home", true), b"\x1bOH");
        assert_eq!(enc("End", false), b"\x1b[F");
    }

    #[test]
    fn modified_arrows_use_csi_param_not_decckm() {
        // modifier param = 1 + shift + 2*alt + 4*ctrl; mode is irrelevant.
        assert_eq!(enc("S-Up", false), b"\x1b[1;2A");
        assert_eq!(enc("S-Up", true), b"\x1b[1;2A");
        assert_eq!(enc("C-Up", false), b"\x1b[1;5A");
        assert_eq!(enc("M-Up", false), b"\x1b[1;3A");
        assert_eq!(enc("C-S-Left", false), b"\x1b[1;6D");
    }

    #[test]
    fn editing_block_and_fkeys() {
        assert_eq!(enc("PPage", false), b"\x1b[5~");
        assert_eq!(enc("NPage", true), b"\x1b[6~"); // unaffected by DECCKM
                                                    // PageUp/PageDown alias PPage/NPage: the page-forward paths emit these
                                                    // names, so the encoder must not drop them on the VT input path.
        assert_eq!(enc("PageUp", false), b"\x1b[5~");
        assert_eq!(enc("PageDown", false), b"\x1b[6~");
        assert_eq!(enc("C-PageUp", false), b"\x1b[5;5~");
        assert_eq!(enc("DC", false), b"\x1b[3~");
        assert_eq!(enc("S-DC", false), b"\x1b[3;2~");
        assert_eq!(enc("F1", false), b"\x1bOP");
        assert_eq!(enc("F4", false), b"\x1bOS");
        assert_eq!(enc("F5", false), b"\x1b[15~");
        assert_eq!(enc("F12", false), b"\x1b[24~");
        assert_eq!(enc("C-F5", false), b"\x1b[15;5~");
    }

    #[test]
    fn simple_keys_and_chords() {
        assert_eq!(enc("Enter", false), b"\r");
        assert_eq!(enc("Tab", false), b"\t");
        assert_eq!(enc("BTab", false), b"\x1b[Z");
        assert_eq!(enc("BSpace", false), vec![0x7f]);
        assert_eq!(enc("Escape", false), vec![0x1b]);
        assert_eq!(enc("Space", false), b" ");
        assert_eq!(enc("C-Space", false), vec![0x00]);
        assert_eq!(enc("C-c", false), vec![0x03]);
        assert_eq!(enc("C-a", false), vec![0x01]);
        assert_eq!(enc("M-x", false), b"\x1bx");
        assert_eq!(enc("M-Enter", false), b"\x1b\r");
    }

    #[test]
    fn action_literal_and_repeat() {
        assert_eq!(
            encode_action_bytes(&TmuxAction::Literal("hi".into()), false),
            b"hi"
        );
        assert_eq!(
            encode_action_bytes(
                &TmuxAction::NamedRepeat {
                    name: "Up".into(),
                    count: 3
                },
                false
            ),
            b"\x1b[A\x1b[A\x1b[A"
        );
        // Resize is never pane input -> empty here (handled by the fork path).
        assert!(encode_action_bytes(&TmuxAction::Resize { cols: 80, rows: 24 }, false).is_empty());
    }
}

/// Cap on concurrently in-flight passive-preview send forks. A fast wheel
/// flick fires many notches in quick succession; without a ceiling each would
/// spawn its own detached thread. Eight in flight keeps scroll responsive,
/// and dropping a notch past that under rapid fire is harmless (the user is
/// still scrolling, and the next notch after a slot frees goes through).
const MAX_INFLIGHT_ONESHOT: usize = 8;
static INFLIGHT_ONESHOT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Releases one `INFLIGHT_ONESHOT` slot on drop, so the count is balanced even
/// if the fork thread panics (otherwise a leaked slot would permanently shrink
/// the cap). Constructed inside the spawned closure, so a spawn that never
/// starts must release its reserved slot itself.
struct OneshotSlot;
impl Drop for OneshotSlot {
    fn drop(&mut self) {
        INFLIGHT_ONESHOT.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

/// Forward a single translated key to a tmux pane with a one-shot
/// `tmux send-keys` fork on a detached thread. Used by the passive-preview
/// wheel forward, where there is no long-lived `LiveSendWorker` to enqueue
/// onto: `dispatch_via_fork` blocks on the subprocess, so it must not run on
/// the UI thread. Fire-and-forget; a dropped scroll notch is harmless and a
/// failed fork is logged, not surfaced. Scroll notches carry no ordering
/// relationship to each other, so racing forks are fine, and the fan-out is
/// bounded by `MAX_INFLIGHT_ONESHOT`.
pub(super) fn send_key_oneshot(tmux_name: &str, key: TmuxKey) {
    use std::sync::atomic::Ordering;
    // Reserve a slot first; if we are already at the cap, drop this notch
    // rather than pile another thread on.
    if INFLIGHT_ONESHOT.fetch_add(1, Ordering::AcqRel) >= MAX_INFLIGHT_ONESHOT {
        INFLIGHT_ONESHOT.fetch_sub(1, Ordering::AcqRel);
        return;
    }
    let tmux_name = tmux_name.to_string();
    let action = match key {
        TmuxKey::Literal(s) => TmuxAction::Literal(s),
        TmuxKey::Named(name) => TmuxAction::Named(name),
        TmuxKey::NamedRepeat { name, count } => TmuxAction::NamedRepeat { name, count },
        TmuxKey::HexBytes(bytes) => TmuxAction::HexBytes(bytes),
        TmuxKey::Paste(text) => TmuxAction::Paste(text),
    };
    // `Builder::spawn` returns the OS error instead of panicking (`spawn`
    // panics if the OS refuses a new thread), so a thread-creation failure
    // under load can't take down the UI thread we're called from. The slot is
    // released by the `OneshotSlot` guard inside the closure on completion or
    // panic; if the spawn never starts, release the reserved slot here.
    let spawned = std::thread::Builder::new()
        .name("aoe-wheel-forward".to_string())
        .spawn(move || {
            let _slot = OneshotSlot;
            // A wheel notch is never a paste, so the vt fast path stays open.
            if let Err(err) = dispatch_via_fork(&tmux_name, &action, false, None) {
                tracing::warn!(
                    target: "tui.live_send",
                    error = %err,
                    action = ?action,
                    "passive-preview wheel forward fork failed; notch dropped",
                );
            }
        });
    if spawned.is_err() {
        INFLIGHT_ONESHOT.fetch_sub(1, Ordering::AcqRel);
        tracing::warn!(
            target: "tui.live_send",
            "could not spawn wheel-forward thread; notch dropped",
        );
    }
}

/// Upper bound on the number of bytes encoded into a single
/// `tmux send-keys -H` fork. Each byte becomes one ~2-char hex argument
/// plus its argv pointer (~11 bytes of kernel arg space), and macOS caps
/// `execve` argv+envp at `ARG_MAX` = 256 KiB, so a per-byte encoding of
/// a large paste overflows around 20 KB and fails wholesale with E2BIG.
/// 4 KiB per fork keeps every argv under ~45 KiB, comfortably below the
/// limit on every platform while keeping the fork count low.
/// Split a literal payload into its leading content and the number of
/// trailing `;` bytes. tmux's command parser drops a trailing `;` from a
/// `send-keys -l` payload, reading it as a command separator even after the
/// `--` end-of-options marker, so the trailing run never reaches the pane
/// (#1942). The caller sends `head` on the literal path and the peeled
/// semicolons as raw hex bytes. Embedded and leading semicolons survive
/// `-l` untouched, so only the trailing run is peeled.
fn peel_trailing_semicolons(s: &str) -> (&str, usize) {
    let head = s.trim_end_matches(';');
    (head, s.len() - head.len())
}

/// Send a literal string to the pane via one `tmux send-keys -l --` fork.
/// Used for the head of a payload whose trailing semicolons were peeled
/// off (see the `Literal` arm of [`dispatch_via_fork`]).
fn send_literal(target: &str, s: &str) -> anyhow::Result<()> {
    use std::process::Stdio;
    let mut cmd = crate::tmux::tmux_command();
    cmd.stderr(Stdio::null());
    cmd.args(["send-keys", "-t", target, "-l", "--", s]);
    let status = cmd
        .status()
        .map_err(|e| anyhow::anyhow!("spawn live-send tmux subprocess: {}", e))?;
    if !status.success() {
        anyhow::bail!(
            "live-send tmux subprocess exited non-zero for literal {:?}",
            s
        );
    }
    Ok(())
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
/// `NamedRepeat` through `tmux send-keys -N <count>` (one fork for N
/// presses of the same key), and `HexBytes` through
/// `tmux send-keys -H <byte> <byte> ...` for raw bytes that can't ride a
/// literal payload (control bytes like ESC, CR, TAB, and the
/// bracketed-paste markers).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum TmuxKey {
    Literal(String),
    Named(String),
    /// A named key sent `count` times in a single fork. The wheel-forward
    /// path uses this to deliver a notch's worth of arrow presses without
    /// one fork per press.
    NamedRepeat {
        name: String,
        count: usize,
    },
    HexBytes(Vec<u8>),
    /// A multi-line paste, delivered through tmux's `paste-buffer -p` so
    /// tmux decides whether the receiving program gets bracketed-paste
    /// markers. Never merged with neighbours: it is one discrete paste.
    Paste(String),
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

    // Shift+Enter (and only Shift+Enter) becomes ESC+CR, the readline
    // convention for "meta-Enter = insert newline". Byte-identical to
    // what tmux emits for the named chord `M-Enter` (the existing
    // M-Enter test case in `vt_input_encode_tests::simple_keys_and_chords`
    // asserts the same `\x1b\r`), so agents that accept Alt+Enter as
    // newline (Claude Code, Codex, opencode, ...) need no further
    // mapping. Only reachable when DISAMBIGUATE_ESCAPE_CODES is active
    // on a kitty-protocol-capable terminal (#2362); legacy terminals
    // still deliver bare Enter and fall through to the named-key path
    // below. Strict modifier equality keeps the Ctrl+Shift+Enter and
    // Alt+Shift+Enter chords on the named-key path so future keybinds
    // can rely on `C-S-Enter` etc. HexBytes is chosen over
    // `Named("M-Enter")` because it short-circuits tmux chord-name
    // parsing and matches the byte representation unambiguously across
    // tmux versions.
    if key.code == KeyCode::Enter && key.modifiers == KeyModifiers::SHIFT {
        return LiveDispatch::Send(TmuxKey::HexBytes(vec![0x1b, b'\r']));
    }

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
    fn assert_hex(d: LiveDispatch, expected: &[u8]) {
        match d {
            LiveDispatch::Send(TmuxKey::HexBytes(b)) => assert_eq!(b, expected),
            other => panic!("expected HexBytes({expected:?}), got {other:?}"),
        }
    }

    fn wait_for_latest(worker: &LiveCaptureWorker, timeout: std::time::Duration) -> Option<String> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(value) = worker.take_latest() {
                return Some(value.content.clone());
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    fn assert_no_latest_for(
        worker: &LiveCaptureWorker,
        timeout: std::time::Duration,
        message: &str,
    ) {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(value) = worker.take_latest() {
                panic!("{message}: got {value:?}");
            }
            if std::time::Instant::now() >= deadline {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
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
    fn shift_enter_emits_esc_cr_hex_bytes() {
        // Shift+Enter on a kitty-protocol-capable terminal lands here
        // (#2362). The agent in the pane reads ESC+CR as the
        // readline meta-Enter "insert newline" convention, identical
        // to how Alt+Enter / M-Enter already works today on terminals
        // that pre-encode Shift+Enter as ESC+CR (Ghostty default).
        assert_hex(
            translate(k_mod(KeyCode::Enter, KeyModifiers::SHIFT)),
            b"\x1b\r",
        );
    }

    #[test]
    fn bare_enter_still_named() {
        // Plain Enter must stay on the named-key path so it continues
        // to deliver bare CR to the agent (= submit). Regression guard
        // against accidentally widening the strict-mod match.
        assert_named(translate(k(KeyCode::Enter)), "Enter");
    }

    #[test]
    fn alt_enter_still_named_m_enter() {
        // Alt+Enter (legacy path: terminals that pre-encode Shift+Enter
        // as ESC+CR deliver Enter+ALT) must continue to produce the
        // named `M-Enter` chord, which tmux expands to ESC+CR. The
        // kitty-protocol fix adds a parallel path for Shift+Enter; it
        // must not displace this one.
        assert_named(
            translate(k_mod(KeyCode::Enter, KeyModifiers::ALT)),
            "M-Enter",
        );
    }

    #[test]
    fn ctrl_shift_enter_falls_through_to_named() {
        // Strict modifier equality means C-S-Enter does NOT hit the
        // HexBytes arm; it stays on the named-key path so a future
        // keybind can target `C-S-Enter` distinctly from Shift+Enter.
        assert_named(
            translate(k_mod(
                KeyCode::Enter,
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            )),
            "C-S-Enter",
        );
    }

    #[test]
    fn alt_shift_enter_falls_through_to_named() {
        // Symmetric to `ctrl_shift_enter_falls_through_to_named`: any
        // modifier set beyond SHIFT alone is rejected by strict equality
        // and falls through to the named-key path so a future keybind
        // can target `M-S-Enter` distinctly from plain Shift+Enter.
        assert_named(
            translate(k_mod(
                KeyCode::Enter,
                KeyModifiers::ALT | KeyModifiers::SHIFT,
            )),
            "M-S-Enter",
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

    fn snd_named_repeat(name: &str, count: usize) -> WorkerMsg {
        WorkerMsg::Send(TmuxKey::NamedRepeat {
            name: name.into(),
            count,
        })
    }

    #[test]
    fn coalesce_same_named_repeats_fold_counts() {
        // Several wheel notches drained in one batch collapse to a single
        // `send-keys -N <total>` fork.
        let out = coalesce(vec![snd_named_repeat("Up", 3), snd_named_repeat("Up", 3)]);
        assert_eq!(
            out,
            vec![TmuxAction::NamedRepeat {
                name: "Up".into(),
                count: 6,
            }]
        );
    }

    #[test]
    fn coalesce_different_named_repeats_do_not_fold() {
        // A direction change (Up then Down) must not merge; the counts and
        // order have to survive so the agent scrolls each way in turn.
        let out = coalesce(vec![snd_named_repeat("Up", 3), snd_named_repeat("Down", 3)]);
        assert_eq!(
            out,
            vec![
                TmuxAction::NamedRepeat {
                    name: "Up".into(),
                    count: 3,
                },
                TmuxAction::NamedRepeat {
                    name: "Down".into(),
                    count: 3,
                },
            ]
        );
    }

    #[test]
    fn coalesce_named_repeat_flushes_literal_run() {
        // Order must hold: literals typed before the wheel notch flush
        // ahead of the arrow repeat, never after it.
        let out = coalesce(vec![
            snd_lit("ab"),
            snd_named_repeat("Down", 3),
            snd_lit("cd"),
        ]);
        assert_eq!(
            out,
            vec![
                TmuxAction::Literal("ab".into()),
                TmuxAction::NamedRepeat {
                    name: "Down".into(),
                    count: 3,
                },
                TmuxAction::Literal("cd".into()),
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
    fn coalesce_paste_breaks_literal_run_and_never_merges() {
        // A paste must stay its own action: folding it into a
        // neighbouring literal run would put the payload back on the
        // `send-keys` path, where tmux never gets to decide about the
        // bracketed-paste markers (the `00~` / `01~` bug), and would
        // also drop the #1546 one-paste framing for agents that DO set
        // DECSET 2004. Ordering across the batch has to survive too.
        let out = coalesce(vec![
            snd_lit("a"),
            snd_lit("b"),
            WorkerMsg::Send(TmuxKey::Paste("x\ny".into())),
            snd_lit("c"),
            WorkerMsg::Send(TmuxKey::Paste("p\nq".into())),
        ]);
        assert_eq!(
            out,
            vec![
                TmuxAction::Literal("ab".into()),
                TmuxAction::Paste("x\ny".into()),
                TmuxAction::Literal("c".into()),
                TmuxAction::Paste("p\nq".into()),
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
    fn peel_trailing_semicolons_splits_trailing_run_only() {
        // tmux eats a trailing `;` from a `send-keys -l` payload, so the
        // dispatcher peels the trailing run and sends it as raw hex (#1942).
        // Lone, trailing, and multi-trailing semicolons get peeled.
        assert_eq!(peel_trailing_semicolons(";"), ("", 1));
        assert_eq!(peel_trailing_semicolons("ls;"), ("ls", 1));
        assert_eq!(peel_trailing_semicolons(";;"), ("", 2));
        assert_eq!(peel_trailing_semicolons("a;;"), ("a", 2));
        // Embedded and leading semicolons survive `-l`, so they stay on the
        // literal head and nothing is peeled.
        assert_eq!(peel_trailing_semicolons("a;b"), ("a;b", 0));
        assert_eq!(peel_trailing_semicolons(";a"), (";a", 0));
        assert_eq!(peel_trailing_semicolons("hello"), ("hello", 0));
        assert_eq!(peel_trailing_semicolons(""), ("", 0));
    }

    #[test]
    fn live_capture_worker_idle_until_geometry_set() {
        // With no line count published the worker must not capture at all,
        // so nothing crosses the channel. (`capture_lines == 0` guard.)
        let worker = LiveCaptureWorker::spawn(std::sync::Arc::new(tokio::sync::Notify::new()));
        worker.set_target("aoe_test_capture_no_geometry".into());
        assert_no_latest_for(
            &worker,
            std::time::Duration::from_millis(500),
            "worker should stay idle until set_capture_lines is called",
        );
    }

    #[test]
    fn live_capture_worker_skips_empty_captures() {
        // A worker pointed at a session that doesn't exist captures empty
        // strings. Forwarding those would blank the preview, defeating the
        // #1501 kill switch, so the worker must drop them. Deterministic
        // without a real tmux session: a missing pane always reads empty.
        let worker = LiveCaptureWorker::spawn(std::sync::Arc::new(tokio::sync::Notify::new()));
        worker.set_target("aoe_test_capture_missing_session".into());
        // Fast cadence so the worker actually captures and drops the empty
        // frame during the polling window.
        worker.set_live(true);
        worker.set_capture_lines(40);
        assert_no_latest_for(
            &worker,
            std::time::Duration::from_millis(500),
            "empty captures must never be forwarded",
        );
    }

    #[test]
    fn stale_generation_helper_rejects_frame_at_generation_zero() {
        let worker = LiveCaptureWorker::spawn(std::sync::Arc::new(tokio::sync::Notify::new()));
        assert_eq!(worker.current_generation_for_test(), 0);

        worker.inject_stale_generation_frame_for_test(40, "previous pane bytes");
        let frame = worker.take_latest().expect("injected capture frame");
        assert_eq!(frame.generation, u64::MAX);
        assert!(!worker.frame_is_current(&frame));
    }

    #[test]
    fn capture_target_change_detects_aba_generation() {
        assert!(!capture_target_changed("a", 7, "a", 7));
        assert!(capture_target_changed("a", 7, "b", 8));
        assert!(
            capture_target_changed("a", 7, "a", 9),
            "A -> B -> A between worker cycles must still reset dedup"
        );
    }

    #[test]
    fn cursor_only_change_requires_atomic_frame_publish() {
        let cursor = crate::tmux::PaneCursor {
            x: 1,
            y: 2,
            visible: true,
            pane_height: 24,
            history_size: 0,
            pane_width: 80,
            alternate_on: false,
            mouse_tracking: false,
            mouse_sgr: false,
            mouse_all: false,
            position_reliable: true,
            composite_pane0: None,
        };
        assert!(frame_needs_publish(false, None, Some(cursor), false));
        assert!(!frame_needs_publish(
            false,
            Some(cursor),
            Some(cursor),
            false
        ));
    }
    #[test]
    fn retarget_invalidates_capture_and_clipboard_mailboxes() {
        // Swapping the target must clear any queued capture so the render
        // never applies the previous pane's bytes under the new view.
        let worker = LiveCaptureWorker::spawn(std::sync::Arc::new(tokio::sync::Notify::new()));
        worker.set_capture_lines(40);
        if let Ok(mut latest) = worker.latest.lock() {
            *latest = Some(CaptureFrame {
                generation: 0,
                target: String::new(),
                budget: 40,
                content: "stale previous-pane content".to_string(),
                cursor: None,
            });
        }
        worker.set_target("aoe_test_capture_new_target".into());
        assert!(
            worker.take_latest().is_none(),
            "retarget must drop the previous pane's queued capture",
        );

        let generation = worker.current_generation_for_test();
        if let Ok(mut clipboard) = worker.clipboard.lock() {
            *clipboard = Some(ClipboardFrame {
                generation: generation.wrapping_sub(1),
                target: String::new(),
                text: "stale secret".to_string(),
            });
        }
        assert!(
            worker.take_agent_clipboard().is_none(),
            "an in-flight old-target publish must be rejected after retarget",
        );

        if let Ok(mut clipboard) = worker.clipboard.lock() {
            *clipboard = Some(ClipboardFrame {
                generation,
                target: "aoe_test_capture_new_target".to_string(),
                text: "current copy".to_string(),
            });
        }
        assert_eq!(
            worker.take_agent_clipboard().as_deref(),
            Some("current copy"),
        );
    }
    #[test]
    fn capture_wake_before_park_stays_pending() {
        let wake: CaptureWake =
            std::sync::Arc::new((std::sync::Mutex::new(0), std::sync::Condvar::new()));
        let mut observed = 0;
        signal_capture_wake(&wake);

        assert!(wait_for_capture_wake(
            &wake,
            &mut observed,
            std::time::Duration::from_secs(1),
        ));
        assert_eq!(observed, 1);
    }

    #[test]
    fn authoritative_capture_reopens_at_trust_ceiling() {
        let t0 = std::time::Instant::now();
        let regular_due = t0 + AUTHORITATIVE_CAPTURE_INTERVAL;
        assert!(authoritative_capture_due(None, t0));
        assert!(!authoritative_capture_due(
            Some(regular_due),
            regular_due - std::time::Duration::from_millis(1)
        ));
        assert!(authoritative_capture_due(Some(regular_due), regular_due));
        assert!(authoritative_refresh_is_quiet(None));
        assert!(!authoritative_refresh_is_quiet(Some((
            AUTHORITATIVE_REFRESH_QUIESCENCE_MS - 1,
            u64::MAX,
        ))));
        assert!(authoritative_refresh_is_quiet(Some((
            AUTHORITATIVE_REFRESH_QUIESCENCE_MS,
            0,
        ))));
        use crate::tmux::vt::VtRefreshResult;
        let cases = [
            (false, None, AuthoritativeRefreshAction::None),
            (
                true,
                Some(VtRefreshResult::Busy),
                AuthoritativeRefreshAction::Schedule(AUTHORITATIVE_CAPTURE_INTERVAL),
            ),
            (
                true,
                Some(VtRefreshResult::Refreshed),
                AuthoritativeRefreshAction::Schedule(AUTHORITATIVE_CAPTURE_INTERVAL),
            ),
            (
                true,
                Some(VtRefreshResult::Failed),
                AuthoritativeRefreshAction::Schedule(VT_REARM_INTERVAL),
            ),
            (
                true,
                None,
                AuthoritativeRefreshAction::Schedule(VT_REARM_INTERVAL),
            ),
        ];
        for (due, result, expected) in cases {
            assert_eq!(authoritative_refresh_action(due, result), expected);
        }
    }
    #[test]
    fn publish_floor_first_change_after_quiet_publishes_immediately() {
        // The typed-echo case: no prior publish (or one long past) must never
        // wait. Reintroducing a wait here re-creates the live-mode echo lag
        // the event-driven wakeup exists to kill.
        assert_eq!(publish_floor_wait_ms(None), 0);
        assert_eq!(
            publish_floor_wait_ms(Some(LIVE_CAPTURE_INTERVAL_FAST_MS)),
            0
        );
        assert_eq!(publish_floor_wait_ms(Some(1_000)), 0);
    }

    #[test]
    fn publish_floor_paces_sustained_streaming_at_fast_cadence() {
        // Back-to-back changes must not publish faster than the fast
        // interval: the 33ms render ticker and its cooldown were calibrated
        // against that pacing, and faster publishes tear on terminals
        // without synchronized updates.
        assert_eq!(
            publish_floor_wait_ms(Some(0)),
            LIVE_CAPTURE_INTERVAL_FAST_MS
        );
        assert_eq!(
            publish_floor_wait_ms(Some(5)),
            LIVE_CAPTURE_INTERVAL_FAST_MS - 5
        );
    }

    #[test]
    fn sample_debounce_lone_chunk_never_waits() {
        // A lone chunk (a keystroke echo, or the first chunk after a quiet gap)
        // reports `streaming == false`, so it must sample with zero added delay
        // regardless of how recently it landed. Adding a wait here re-creates
        // the live-mode echo lag the #2822 event-driven wakeup exists to kill.
        assert_eq!(sample_debounce_wait_ms(false, 0, 0), 0);
        assert_eq!(sample_debounce_wait_ms(false, 0, 100), 0);
        assert_eq!(sample_debounce_wait_ms(false, 3, 0), 0);
    }

    #[test]
    fn sample_debounce_holds_active_stream_until_quiescent() {
        // While chunks are arriving back-to-back and the stream has not gone
        // quiet, a changed frame is held so a multi-chunk repaint publishes once
        // it settles instead of mid-repaint. The wait is the remaining
        // quiescence window.
        assert_eq!(
            sample_debounce_wait_ms(true, 0, 0),
            SAMPLE_QUIESCENCE_MS,
            "a fresh stream chunk waits the full quiescence window",
        );
        assert_eq!(
            sample_debounce_wait_ms(true, SAMPLE_QUIESCENCE_MS - 2, 10),
            2,
            "the wait shrinks to the quiescence remainder",
        );
    }

    #[test]
    fn sample_debounce_publishes_once_stream_goes_quiet() {
        // Once the stream has been silent for the quiescence window, the
        // settled frame publishes immediately (this is the repaint's final
        // frame arriving right after output stops).
        assert_eq!(sample_debounce_wait_ms(true, SAMPLE_QUIESCENCE_MS, 10), 0);
        assert_eq!(
            sample_debounce_wait_ms(true, SAMPLE_QUIESCENCE_MS + 5, 10),
            0
        );
    }

    #[test]
    fn sample_debounce_latency_cap_bounds_sustained_streaming() {
        // A stream that never goes quiet must still render: once a held frame
        // has waited out the latency cap it publishes regardless of how
        // recently the last chunk landed, so heavy output paces at the cap
        // rather than stalling until it happens to pause.
        assert_eq!(sample_debounce_wait_ms(true, 0, SAMPLE_LATENCY_CAP_MS), 0);
        assert_eq!(
            sample_debounce_wait_ms(true, 0, SAMPLE_LATENCY_CAP_MS + 100),
            0
        );
        // Just under the cap, the wait is clamped so it can never overshoot it.
        assert_eq!(
            sample_debounce_wait_ms(true, 0, SAMPLE_LATENCY_CAP_MS - 1),
            1,
        );
    }

    #[test]
    fn resize_batches_require_verified_ownership_before_dispatch() {
        // Keystroke batches must dispatch without waiting on the size-owner
        // check (a few tmux forks); putting it back ahead of plain input
        // re-creates the per-keystroke latency this classifier exists to
        // avoid. Resizes keep verify-first so geometry never races another
        // owner's grid.
        assert!(batch_needs_owner_first(&[WorkerMsg::Resize {
            cols: 80,
            rows: 24
        }]));
        assert!(batch_needs_owner_first(&[
            WorkerMsg::Send(TmuxKey::Literal("a".into())),
            WorkerMsg::Resize { cols: 80, rows: 24 },
        ]));
        assert!(!batch_needs_owner_first(&[
            WorkerMsg::Send(TmuxKey::Literal("abc".into())),
            WorkerMsg::Send(TmuxKey::Named("Enter".into())),
            WorkerMsg::Send(TmuxKey::HexBytes(vec![0x1b])),
        ]));
        assert!(!batch_needs_owner_first(&[]));

        let cases = [
            (true, false, true),
            (false, false, false), // Unknown or vacant after a failed claim.
            (true, true, false),
            (false, true, false),
        ];
        for (owned, lock_lost, expected) in cases {
            assert_eq!(
                resize_dispatch_authorized(owned, lock_lost),
                expected,
                "owned={owned}, lock_lost={lock_lost}",
            );
        }
    }

    #[test]
    fn live_capture_worker_forwards_empty_when_policy_set() {
        // Terminal / container panes set `forward_empty`, so a missing or
        // cleared pane must surface as an empty capture (clearing stale
        // preview text) instead of being dropped like the agent kill switch.
        // Deterministic without a real tmux session: a missing pane reads empty.
        let worker = LiveCaptureWorker::spawn(std::sync::Arc::new(tokio::sync::Notify::new()));
        worker.set_target("aoe_test_capture_forward_empty".into());
        worker.set_forward_empty(true);
        // Fast cadence so the worker captures during the polling window.
        worker.set_live(true);
        worker.set_capture_lines(40);
        assert_eq!(
            wait_for_latest(&worker, std::time::Duration::from_secs(2)),
            Some(String::new()),
            "forward-empty policy must surface empty captures",
        );
    }

    #[test]
    fn live_capture_worker_publishes_failure_as_empty_outside_live() {
        // Regression (worker-only cutover): when the displayed agent/tool
        // pane DIES, the capture fails instead of returning empty content,
        // and only `forward_empty` panes used to surface that. Outside
        // live-send a failed capture must publish an empty frame so the
        // preview shows "No output available" instead of the dead pane's
        // last bytes forever. Deterministic without tmux: a missing pane
        // always fails its capture. Live mode keeps the #1501 kill switch.
        let worker = LiveCaptureWorker::spawn(std::sync::Arc::new(tokio::sync::Notify::new()));
        worker.set_target("aoe_test_capture_dead_agent".into());
        worker.set_capture_lines(40);
        assert_eq!(
            wait_for_latest(&worker, std::time::Duration::from_secs(2)),
            Some(String::new()),
            "a failed capture outside live must surface as an empty frame",
        );
    }

    #[test]
    fn live_capture_worker_republishes_on_budget_change() {
        // A budget change alone (deeper scroll over a quiet pane) must
        // republish even when the captured bytes are identical: consumers
        // waiting for a wider/deeper capture would otherwise stall forever.
        // Deterministic without tmux: forward-empty + missing pane produces
        // identical empty content at every budget.
        let worker = LiveCaptureWorker::spawn(std::sync::Arc::new(tokio::sync::Notify::new()));
        worker.set_target("aoe_test_capture_budget_change".into());
        worker.set_forward_empty(true);
        worker.set_live(true);
        worker.set_capture_lines(40);
        assert_eq!(
            wait_for_latest(&worker, std::time::Duration::from_secs(2)),
            Some(String::new()),
            "first capture publishes",
        );
        worker.set_capture_lines(80);
        assert_eq!(
            wait_for_latest(&worker, std::time::Duration::from_secs(2)),
            Some(String::new()),
            "budget change alone must republish identical bytes",
        );
    }

    fn tmux_available() -> bool {
        crate::tmux::tmux_command()
            .arg("-V")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn wait_until(what: &str, timeout: std::time::Duration, mut cond: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            if cond() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        panic!("timed out waiting for {what}");
    }

    #[test]
    fn live_send_worker_reports_failed_resize() {
        let name = "aoe_test_missing_live_resize";
        let action = TmuxAction::Resize { cols: 80, rows: 24 };
        assert!(
            dispatch_via_fork(name, &action, false, Some("test-owner")).is_err(),
            "the bounded resize path must expose failure"
        );

        let worker = LiveSendWorker::spawn(name.to_string(), None);
        worker.resize(80, 24);
        wait_until(
            "live resize failure flag",
            std::time::Duration::from_secs(5),
            || {
                worker
                    .resize_failed
                    .load(std::sync::atomic::Ordering::Relaxed)
            },
        );
        assert!(
            worker.take_resize_failed(),
            "paint consumes the sticky failure"
        );
        assert!(!worker.take_resize_failed(), "consuming the flag clears it");
    }

    fn pane_width(name: &str) -> u16 {
        let out = crate::tmux::tmux_command()
            .args([
                "display-message",
                "-p",
                "-t",
                &format!("{name}:^.0"),
                "-F",
                "#{pane_width}",
            ])
            .output()
            .expect("tmux display-message");
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .unwrap_or(0)
    }

    /// The worker never steals the size-owner lock back after entry: an
    /// external steal (a web "take over") flips its sticky `lock_lost`
    /// flag, the thief keeps the lock, and a queued resize is dropped
    /// instead of stomping the new owner's grid. This is the fix for the
    /// silent tug-of-war where a background TUI's next keystroke or
    /// preview-rect jitter reverted a phone takeover.
    #[test]
    #[serial_test::serial]
    fn worker_flags_lock_loss_and_drops_resize_after_external_steal() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }
        let guard = crate::tmux::test_helpers::TmuxTestSession::new("aoe_test_livelock_steal");
        let out = crate::tmux::tmux_command()
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
        assert!(out.status.success());
        crate::tmux::refresh_session_cache();
        let session = crate::tmux::Session::from_name(guard.name());

        let worker = LiveSendWorker::spawn(guard.name().to_string(), None);
        wait_until(
            "worker entry steal",
            std::time::Duration::from_secs(5),
            || matches!(session.size_owner(), Some((id, _)) if id.starts_with("tui-")),
        );
        assert!(!worker.lock_lost());

        // A web live viewer takes over (what live_ws's Claim handler does).
        assert!(session.steal_size_owner("live-test-thief"));

        // The next resize must verify, observe the loss, flag it, and be
        // dropped. (The idle heartbeat may flag it first; either path is
        // the behavior under test.)
        worker.resize(60, 20);
        wait_until("lock_lost flag", std::time::Duration::from_secs(5), || {
            worker.lock_lost()
        });
        assert_eq!(
            session.size_owner().map(|(id, _)| id),
            Some("live-test-thief".to_string()),
            "worker must not steal the lock back"
        );
        // Give any (wrongly) dispatched resize time to land, then confirm
        // the pane kept the thief's geometry.
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert_eq!(
            pane_width(guard.name()),
            80,
            "dropped resize must not dispatch"
        );
    }

    /// Control case: while the worker still owns the lock, resizes verify
    /// successfully and dispatch as before.
    #[test]
    #[serial_test::serial]
    fn worker_resizes_while_it_owns_the_lock() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }
        let guard = crate::tmux::test_helpers::TmuxTestSession::new("aoe_test_livelock_own");
        let out = crate::tmux::tmux_command()
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
        assert!(out.status.success());
        crate::tmux::refresh_session_cache();
        let session = crate::tmux::Session::from_name(guard.name());

        let worker = LiveSendWorker::spawn(guard.name().to_string(), None);
        wait_until(
            "worker entry steal",
            std::time::Duration::from_secs(5),
            || matches!(session.size_owner(), Some((id, _)) if id.starts_with("tui-")),
        );
        worker.resize(60, 20);
        wait_until(
            "owned resize dispatch",
            std::time::Duration::from_secs(5),
            || pane_width(guard.name()) == 60,
        );
        assert!(!worker.lock_lost());
    }

    /// The entry steal can come up empty two ways: the pane has not appeared
    /// yet, or another surface won the confirm-read race. The retry path used
    /// to force-steal for both, which silently stomped a live owner and never
    /// flagged the loss. Spawning before the session exists reproduces the
    /// `owned == false` entry deterministically, without racing tmux forks.
    #[test]
    #[serial_test::serial]
    fn worker_defers_to_live_owner_when_entry_steal_found_no_session() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }
        let guard = crate::tmux::test_helpers::TmuxTestSession::new("aoe_test_livelock_late");
        // Spawn against a name that does not exist yet: the entry steal sees
        // no session and leaves the worker unowned.
        let worker = LiveSendWorker::spawn(guard.name().to_string(), None);
        std::thread::sleep(std::time::Duration::from_millis(500));

        let out = crate::tmux::tmux_command()
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
        assert!(out.status.success());
        crate::tmux::refresh_session_cache();
        let session = crate::tmux::Session::from_name(guard.name());

        // Another surface owns the pane before the worker ever retries.
        assert!(session.steal_size_owner("live-test-thief"));
        assert!(!worker.lock_lost());

        // The retry must claim, not steal: a live holder is a takeover, so the
        // loss is flagged and the resize dropped.
        worker.resize(60, 20);
        wait_until("lock_lost flag", std::time::Duration::from_secs(5), || {
            worker.lock_lost()
        });
        assert_eq!(
            session.size_owner().map(|(id, _)| id),
            Some("live-test-thief".to_string()),
            "unowned retry must not steal from a live owner"
        );
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert_eq!(
            pane_width(guard.name()),
            80,
            "dropped resize must not dispatch"
        );
    }

    /// The same retry path must still take a vacant lock, so a genuinely
    /// slow-to-appear pane gets owned instead of being abandoned.
    #[test]
    #[serial_test::serial]
    fn worker_claims_vacant_lock_when_session_appears_late() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }
        let guard = crate::tmux::test_helpers::TmuxTestSession::new("aoe_test_livelock_vacant");
        let worker = LiveSendWorker::spawn(guard.name().to_string(), None);
        std::thread::sleep(std::time::Duration::from_millis(500));

        let out = crate::tmux::tmux_command()
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
        assert!(out.status.success());
        crate::tmux::refresh_session_cache();
        let session = crate::tmux::Session::from_name(guard.name());

        worker.resize(60, 20);
        wait_until(
            "late resize dispatch",
            std::time::Duration::from_secs(5),
            || pane_width(guard.name()) == 60,
        );
        assert!(!worker.lock_lost());
        assert!(
            matches!(session.size_owner(), Some((id, _)) if id.starts_with("tui-")),
            "vacant lock must still be claimed on the retry path"
        );
    }
}
