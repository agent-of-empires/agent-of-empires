//! Live-send mode: a "feels-attached" alternative to the compose dialog.
//!
//! `Tab` on a runnable session installs a `LiveSendState` and routes every key event
//! through this module's translator, which produces a `TmuxAction` for whichever
//! transport the pane has armed.
//!
//! While a VT channel carries input (`[tmux] vt_live`, tmux 3.8+, unix) the action is
//! encoded to raw terminal bytes and written into the pane socket, bypassing tmux's key
//! translation, so the encoder honors the pane's DECCKM itself. That is the default, and
//! a live input channel is a single-writer signal: two writers would interleave on one
//! pty input stream, so nothing goes through `send-keys` while it exists.
//!
//! Otherwise each action forks `tmux send-keys`: plain characters literally, every other
//! key by tmux key name with `C-` / `M-` prefixes. That covers tmux older than 3.8 (where
//! writing to a dead pane's input takes the server down), a pane whose forwarder never
//! connected, and `vt_live` turned off. A long-lived `tmux -C` connection was tried
//! (#1485) and EOF'd within milliseconds on macOS tmux 3.x, so one fork per coalesced
//! batch is the portable model; held keys and pastes coalesce into one fork.
//!
//! The user exits with one of the configured exit chords, a comma-separated list of chord
//! specs whose default is `C-q` alone: mobile-friendly, passes through restrictive SSH
//! clients, and leaves every other chord for the agent. A bound chord cannot be sent
//! through, so users who need `C-q` downstream configure a different exit.
//!
//! There is no echo, inline editing or review step; the preview pane is the only feedback,
//! and multi-line composition belongs in the compose dialog on `M`.
//!
//! Other reserved chords: `Shift+PageUp` / `Shift+PageDown` scroll the preview back
//! through agent history (bare `PageUp` / `PageDown` still passes through), and the mouse
//! wheel over the preview scrolls it through `handle_scroll_up` / `handle_scroll_down`.

use std::sync::mpsc::{channel, Sender};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Distinguishes successive live-send workers in the size-owner lock, so a rapid toggle's
/// old worker can't release a lock the new one re-stole. Process-local; the pid gives
/// cross-process uniqueness.
static LIVE_SEND_WORKER_COUNTER: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
#[cfg(test)]
static LIVE_CAPTURE_WORKER_TEST_COUNTER: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Default exit chord set: `C-q` alone, which works on mobile and restrictive SSH
/// clients, is reachable on every shipped keyboard layout, and reads as "quit". Users who
/// need `C-q` downstream configure another chord, or a comma-separated list.
///
/// `Ctrl+]` and `Ctrl+\` were each tried and silently failed on at least one common
/// macOS terminal/keyboard combination, so the default is one chord rather than a
/// two-hand exit that looks like it should work. Settings saved under 1.9.0 have
/// `"C-q,C-]"` in config.toml; re-saving restores this default.
pub(super) const DEFAULT_EXIT_CHORD: &str = "C-q";

/// Default live-send leader (prefix) chord. `Ctrl+b` matches tmux and herdr, so
/// multiplexer users have the muscle memory, and it is the one chord stolen from the
/// agent (double-tap still delivers a literal `C-b`). Kept in sync with
/// `default_live_send_leader()` in `session::config`; an empty value disables the leader.
pub(super) const DEFAULT_LEADER: &str = "C-b";

/// Parse a tmux-style chord spec into `(KeyCode, KeyModifiers)`. Accepts `C-` / `Ctrl-`,
/// `M-` / `Alt-`, `S-` / `Shift-` prefixes in any order, separated by `-` or `+`, followed
/// by a single ASCII char or a tmux key name (`Escape`, `Tab`, `BTab`, arrows, `Enter`,
/// `BSpace`, `DC`, `IC`, `Home`, `End`, `PPage`, `NPage`, `Space`, `F1`..`F12`).
///
/// `None` on parse failure, so the caller can fall back to the default and warn. Char keys
/// lowercase under any modifier, so `C-q` and `C-Q` share one canonical form.
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
            // Single-char key: drop case sensitivity when a modifier is held, as tmux
            // treats Ctrl+a and Ctrl+A alike. Unmodified, case is preserved so a
            // configured "Q" means uppercase Q.
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

/// True when `event` is a configured exit chord. Char codes normalize under Ctrl (the
/// canonical form `parse_chord` produces); modifiers match strictly otherwise, so `C-q`
/// does not fire on `Ctrl+Shift+q` and the user can still deliver `C-q` to the agent.
pub(super) fn chord_matches(spec: (KeyCode, KeyModifiers), event: KeyEvent) -> bool {
    let mut event_code = event.code;
    if event.modifiers.contains(KeyModifiers::CONTROL) {
        if let KeyCode::Char(c) = event_code {
            event_code = KeyCode::Char(c.to_ascii_lowercase());
        }
    }
    spec.0 == event_code && spec.1 == event.modifiers
}

/// Parse a comma-separated chord list (`"C-q,F12"`) into the pairs the exit check
/// compares against. Invalid pieces are dropped with a warning so one typo doesn't
/// disable the list, and an entirely unparseable string falls back to the default, so the
/// user is never trapped in live mode.
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

/// Render the configured chord list for the banner, e.g. `"Ctrl+Q / F12"`, so the user
/// sees every chord that exits.
pub(super) fn display_chord_list(chords: &[(KeyCode, KeyModifiers)]) -> String {
    chords
        .iter()
        .map(|c| display_chord(*c))
        .collect::<Vec<_>>()
        .join(" / ")
}

/// Render a parsed chord as a human-readable string for the banner, uppercasing letters
/// so it reads like the TUI's other chord hints.
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

/// Lives on `HomeView::live_send` while the mode is active, carrying enough state for the
/// banner, for the exit handler to confirm the targeted pane, and for the per-keystroke
/// liveness check: `tmux_name` is the entry-time value, so a diverging
/// `generate_name(id, title)` auto-exits rather than sending into the void.
// `pub(in crate::tui)` matches HomeView's field, whose `pub(super)` resolves to the same
// scope from mod.rs: tighter trips `private_interfaces`, looser leaks the type.
#[derive(Debug, Clone)]
pub(in crate::tui) struct LiveSendState {
    pub session_id: String,
    pub title: String,
    pub tmux_name: String,
    /// Which paired pane the live-send targets, captured at entry so drift checks,
    /// exit sizing and view-mode flips can't move where keystrokes go.
    pub target: LiveSendTarget,
    /// Exit chords parsed at entry, so config edits don't change behavior mid-session.
    pub exit_chords: Vec<(KeyCode, KeyModifiers)>,
    /// Leader chord parsed at entry, `None` when the user cleared the setting (every key
    /// passes through). When set, the first press arms the live-send command menu and the
    /// next key picks a command, while a second leader press passes a literal through.
    pub leader: Option<(KeyCode, KeyModifiers)>,
}

/// Which paired tmux pane a live-send dispatch targets: the agent pane is the historical
/// default, and the host and container terminal panes reuse the same machinery.
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

/// Format a `(title, target)` label so the compose dialog header and the live banner stay
/// in lockstep: Agent keeps the bare title, terminal variants get a short parenthetical
/// naming the pane the keystrokes land on.
pub(in crate::tui) fn format_target_label(title: &str, target: &LiveSendTarget) -> String {
    match target {
        LiveSendTarget::Agent => title.to_string(),
        LiveSendTarget::Terminal => format!("{title} (terminal)"),
        LiveSendTarget::ContainerTerminal => format!("{title} (container)"),
        LiveSendTarget::Tool(name) => format!("{title} ({name})"),
    }
}

/// One coalesced unit of work for tmux. `Literal` runs fold together; named keys, hex-byte
/// runs and resizes break the run because their order matters (an Up arrow between "ab"
/// and "cd" must arrive between them; a resize before keystrokes renders them at the new
/// geometry). Consecutive `HexBytes` do merge, so a multi-blank-line paste is one
/// `send-keys -H`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum TmuxAction {
    Literal(String),
    Named(String),
    /// `send-keys -N <count> <name>`: the named key repeated in one fork. Consecutive
    /// runs of the same key fold their counts together.
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

/// Fold a batch of `WorkerMsg`s into the smallest ordering-preserving sequence of
/// `TmuxAction`s: consecutive `Send(Literal)` merge into one `send-keys -l`, consecutive
/// `Send(HexBytes)` into one `send-keys -H`, and a named key, hex run or resize flushes
/// the literal run. Pure, so ordering is testable without a worker thread.
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
                // Never merged: a paste is one discrete tmux paste-buffer call, and
                // folding it into a run would put the payload back on the literal path.
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

/// Whether a drained batch must verify size ownership before dispatch. Only geometry
/// needs that ordering, since a `resize-window` racing another surface's grid is the flap
/// the size-owner lock exists to kill; keystrokes never read the lock, so they dispatch
/// without waiting on the verify's forks.
pub(super) fn batch_needs_owner_first(batch: &[WorkerMsg]) -> bool {
    batch.iter().any(|m| matches!(m, WorkerMsg::Resize { .. }))
}

fn resize_dispatch_authorized(owned: bool, lock_lost: bool) -> bool {
    owned && !lock_lost
}

/// One unit of work for the worker. Resizes don't coalesce with keys because they are
/// sticky pane-level changes: a burst bracketing a resize must arrive on either side of
/// the geometry change, not be reordered after it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum WorkerMsg {
    Send(TmuxKey),
    Resize { cols: u16, rows: u16 },
}

/// Background dispatcher: drains `WorkerMsg`s and runs each through a one-shot
/// `send-keys` / `resize-window` after `coalesce`. Spawned by `prepare_live_send` and
/// dropped on exit, which closes the channel so the thread's `recv` fails and it exits.
/// Deliberately not joined: the worker is idempotent and harmless if it outlives a rapid
/// live-mode toggle by a moment.
pub(in crate::tui) struct LiveSendWorker {
    tx: Sender<WorkerMsg>,
    /// Set (sticky) by the worker when the size-owner lock is seen held by another
    /// surface. The UI loop polls it and exits live mode; the worker never steals back
    /// after entry, so a web "take over" wins instead of ping-ponging.
    lock_lost: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Set by the worker when resize-window fails or times out. Paint consumes
    /// the flag and schedules a bounded retry for the same geometry.
    resize_failed: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl LiveSendWorker {
    /// `capture_wake`, when present, nudges the preview capture worker out of its
    /// inter-capture wait after each dispatched batch, so typed echo is captured
    /// immediately rather than a full cadence cycle later.
    pub(super) fn spawn(tmux_name: String, capture_wake: Option<LiveCaptureWake>) -> Self {
        let (tx, rx) = channel::<WorkerMsg>();
        let lock_lost = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let thread_lock_lost = std::sync::Arc::clone(&lock_lost);
        let resize_failed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let thread_resize_failed = std::sync::Arc::clone(&resize_failed);

        std::thread::spawn(move || {
            use std::sync::atomic::Ordering;
            use std::sync::mpsc::RecvTimeoutError;

            // Entering live mode is an active take-over: it steals the session's size so
            // the web PTY relay and mobile live view defer to it. Entry is the only steal;
            // afterwards ownership is merely refreshed, and losing it flips `lock_lost` so
            // the UI exits instead of fighting. Re-stealing after entry is how a
            // background TUI used to silently revert a phone takeover on any keystroke or
            // preview-rect jitter. Released (and `window-size latest` restored) on exit;
            // after a lost lock the release is a no-op and the new owner's sizing stands.
            let owner_id = format!(
                "tui-{}-{}",
                std::process::id(),
                LIVE_SEND_WORKER_COUNTER.fetch_add(1, Ordering::Relaxed)
            );
            let session = crate::tmux::Session::from_name(&tmux_name);
            // Entry is explicit user intent, so it forces the lock even over a live
            // holder. False means the session is missing or broken at entry, or another
            // surface won the confirm-read race; the retry on the next resize batch tells
            // those apart.
            let mut owned = session.steal_size_owner(&owner_id);
            // Refresh-or-flag: bump the heartbeat only while we still hold the lock; a
            // failed refresh means another surface took over, flagged once and not fought.
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

            // Block (up to a heartbeat) for the first message, then drain what piled up.
            // The drain plus `coalesce` collapses paste bursts and autorepeat into one
            // fork per literal run.
            //
            // Owner bookkeeping stays off the keystroke path: a steal is ~5 tmux forks and
            // running it ahead of every batch made typing measurably laggier than a direct
            // attach. Keystrokes never read the size lock, so they dispatch first and
            // ownership is re-asserted at most once per heartbeat. Resize batches keep the
            // steal-before-dispatch ordering, since geometry must not race another grid.
            let mut last_owner_maintenance = std::time::Instant::now();
            loop {
                match rx.recv_timeout(crate::tmux::SIZE_OWNER_HEARTBEAT) {
                    Ok(first) => {
                        let mut batch = vec![first];
                        while let Ok(msg) = rx.try_recv() {
                            batch.push(msg);
                        }
                        // A batch carrying a resize verifies ownership first; plain
                        // keystrokes never read the lock.
                        let mut resize_unverified = false;
                        if batch_needs_owner_first(&batch) {
                            if !owned {
                                // Entry steal failed, so claim rather than steal: a
                                // vacant, stale or already-ours lock is still taken (the
                                // slow-to-appear pane), while a live holder means another
                                // surface won and must not be stomped. Re-forcing would
                                // fight a takeover the entry race makes indistinguishable
                                // from a missing pane, and would never flag the loss.
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
                            // A resize without verified ownership could stomp another
                            // surface's grid; keys stay independent of the lock and still
                            // deliver in order.
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
                        // Idle heartbeat: a failed refresh here is usually the earliest
                        // takeover signal (the web steals while the desktop sits idle), so
                        // `maintain` flags it and the UI exits without waiting for input.
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

    /// True once the worker saw the size-owner lock held elsewhere. Sticky for the
    /// worker's lifetime; the UI loop polls it and exits live mode.
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

    /// Enqueue a translated key. Returns immediately: the fork happens on the worker
    /// thread, so the UI never blocks on tmux latency.
    pub(super) fn send(&self, key: TmuxKey) {
        // A send only fails if the worker thread panicked. Drop silently: the user's next
        // exit clears the dead worker and the next entry spawns a fresh one.
        let _ = self.tx.send(WorkerMsg::Send(key));
    }

    /// Enqueue a tmux pane resize, serialized with surrounding keystrokes so keys typed
    /// before it arrive in the old size and keys after in the new one, which matters when
    /// an agent uses cursor-position escapes.
    pub(super) fn resize(&self, cols: u16, rows: u16) {
        let _ = self.tx.send(WorkerMsg::Resize { cols, rows });
    }
}

/// How often the capture worker forks `tmux capture-pane` while idle-ish. It free-runs at
/// roughly this cadence, keeping the preview fresh off the render loop, and sits just
/// under the 33ms render ticker so a frame almost always finds content newer than the last
/// keystroke without raising the steady-state fork rate.
/// Capture cadence while live-send is attached to the displayed pane: tight, for
/// near-attach echo latency. On the VT path it is only the fallback wait (the channel's
/// change wakeup ends the sleep as output lands) and doubles as the floor between
/// published frames, so change-driven sampling can't outpace the 33ms frame pacing.
const LIVE_CAPTURE_INTERVAL_FAST_MS: u64 = 25;

/// Milliseconds a just-changed frame must wait before publishing, given how long ago the
/// previous frame published (`None` = never). Zero publishes now: the first change after a
/// quiet gap is the typed-echo case, while sustained streaming paces at the fast cadence.
/// Keying off publishes rather than samples keeps a wasted pre-echo sample from pushing
/// the real echo back a cycle.
pub(super) fn publish_floor_wait_ms(since_last_publish_ms: Option<u64>) -> u64 {
    match since_last_publish_ms {
        None => 0,
        Some(elapsed) => LIVE_CAPTURE_INTERVAL_FAST_MS.saturating_sub(elapsed),
    }
}

/// Quiescence window for the VT sample debounce: while output streams back-to-back, a
/// changed frame waits for this much silence before publishing, so a clear-then-reprint
/// spanning several chunks publishes once settled instead of mid-repaint (#2903).
const SAMPLE_QUIESCENCE_MS: u64 = 6;

/// Hard cap on how long the sample debounce may hold a changed frame, so a stream that
/// never goes quiet still renders at a bounded cadence.
const SAMPLE_LATENCY_CAP_MS: u64 = 40;

/// How long a just-changed VT frame must wait before sampling, given whether output is
/// `streaming` (chunks within [`SAMPLE_QUIESCENCE_MS`]), `since_last_chunk_ms` and
/// `since_pending_ms`. Zero means publish now.
///
/// A lone chunk (an echo, or the first after a quiet gap) reports `streaming == false` and
/// never waits, leaving the #2822 echo-latency path untouched. Only a live stream defers,
/// and only until it goes quiet or the latency cap fires.
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
/// Capture cadence when the worker only keeps the home-list preview warm. Matches the
/// render-driven throttle it replaces, so moving the fork off the render thread does not
/// raise the idle fork rate.
const LIVE_CAPTURE_INTERVAL_IDLE_MS: u64 = 250;
/// Maximum interval between authoritative snapshots while a live VT grid supplies preview
/// frames, preserving the bounded self-heal for a grid whose cells diverge while cursor
/// and geometry agree, without moving `capture-pane` back onto paint.
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
/// Minimum wait between VT arm attempts for one target. A dead channel (the pane was
/// killed and its session recreated under the same name) heals within this window instead
/// of stranding on the capture fallback, while a permanently un-armable pane costs one
/// cheap failed attempt per interval rather than one per tick.
const VT_REARM_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3);

/// How long a selection must rest on a pane before the worker arms a channel: arming puts
/// a real `pipe-pane` on the pane, so a selection moving through the list must not arm and
/// tear one down per row. Until it rests, each row costs one `capture-pane` fork.
#[cfg(unix)]
const CHANNEL_ARM_SETTLE: std::time::Duration = std::time::Duration::from_millis(250);

/// Whether this cycle may start arming a pane channel (VT grid or OSC 52 observer): the
/// selection has rested for `CHANNEL_ARM_SETTLE`, no channel is held or arming, and the
/// retry throttle has elapsed.
#[cfg(unix)]
fn channel_arm_due(
    arm_after: Option<std::time::Instant>,
    armed_or_pending: bool,
    last_arm: Option<std::time::Instant>,
    now: std::time::Instant,
) -> bool {
    arm_after.is_none_or(|t| now >= t)
        && !armed_or_pending
        && last_arm.is_none_or(|t| now.duration_since(t) >= VT_REARM_INTERVAL)
}

/// A channel arm running off the worker thread, so its chain of tmux forks and the
/// forwarder spawn never delay a frame. The result is tagged with the generation it was
/// started for; a stale one goes to the caller's teardown sink after the cycle's frame.
#[cfg(unix)]
struct PendingArm<T> {
    generation: u64,
    result: std::sync::mpsc::Receiver<Option<std::sync::Arc<T>>>,
}

#[cfg(unix)]
impl<T: Send + Sync + 'static> PendingArm<T> {
    fn spawn(
        name: String,
        generation: u64,
        nudge: CaptureWake,
        arm: impl FnOnce(&str, &crate::tmux::TmuxCommandDeadline) -> Option<std::sync::Arc<T>>
            + Send
            + 'static,
    ) -> Self {
        let (sender, result) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let deadline = crate::tmux::TmuxCommandDeadline::new();
            // A send to a stopped worker fails and drops the channel here.
            let _ = sender.send(arm(&name, &deadline));
            signal_capture_wake(&nudge);
        });
        Self { generation, result }
    }

    /// The channel a finished arm produced for `generation`. `None` while the arm is
    /// running, failed, or finished for another generation, which is pushed onto `stale`.
    fn take(
        pending: &mut Option<Self>,
        generation: u64,
        stale: &mut Vec<std::sync::Arc<T>>,
    ) -> Option<std::sync::Arc<T>> {
        let done = match pending.as_ref()?.result.try_recv() {
            Ok(done) => done,
            Err(std::sync::mpsc::TryRecvError::Empty) => return None,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => None,
        };
        if pending.take().is_some_and(|p| p.generation == generation) {
            done
        } else {
            stale.extend(done);
            None
        }
    }
}

/// Cloneable handle that nudges a [`LiveCaptureWorker`] out of its inter-capture wait, so
/// a dispatched keystroke batch captures the typed echo immediately. Backed by the same
/// condvar as `set_live` / `set_target`, so a wake just runs one capture early.
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

/// One atomic capture frame: content, the cursor probed in the same cycle, the line budget
/// it was captured under, and its target generation. Published as a unit so a consumer can
/// never see a frame torn across a retarget or budget change.
#[derive(Debug)]
pub(in crate::tui) struct CaptureFrame {
    /// Target generation at capture time, compared against the worker's current one so a
    /// frame captured before a retarget is dropped instead of landing under the new view.
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

/// Whether the worker must reset target-scoped dedup and transport state. Generation is
/// part of the identity: A -> B -> A can happen between cycles, leaving the name unchanged
/// but the mailbox cleared.
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
/// Off-thread preview capture: one long-lived thread forks `tmux capture-pane` and
/// publishes into a single-slot mailbox the render loop drains, moving the per-frame
/// capture cost (~8.5ms on macOS, ~90% of a live-send frame) off the hot path. Dropping
/// the worker flips `stop` so the thread exits after its cycle; like `LiveSendWorker` it
/// is not joined.
///
/// It tracks whichever pane the preview displays (agent, terminal, container shell or
/// tool): `sync_preview_capture_worker` points it via `set_target` on every selection or
/// view-mode change, so a switch swaps the target in place instead of spawning a thread.
/// `set_live` adapts the cadence: tight during live-send, `LIVE_CAPTURE_INTERVAL_IDLE_MS`
/// otherwise.
pub(in crate::tui) struct LiveCaptureWorker {
    /// Lines the render loop wants captured (height + scrollback + buffer). `0` means not
    /// set yet and the worker captures nothing; `capture_lines_for` never yields 0.
    capture_lines: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// tmux session name being captured, swapped in place by `set_target` when the
    /// displayed pane changes, so one thread serves every view. Empty means idle.
    target: std::sync::Arc<std::sync::Mutex<String>>,
    /// Single-slot mailbox holding the newest unconsumed [`CaptureFrame`]. A new frame
    /// overwrites an unconsumed one, since the render only wants the latest, so it cannot
    /// grow unbounded if the render thread stalls.
    latest: std::sync::Arc<std::sync::Mutex<Option<CaptureFrame>>>,
    /// Bumped on every `set_target` change. Frames carry the value read at capture time
    /// and consumers drop mismatches, so bytes captured mid-switch never land under a new
    /// view.
    generation: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Sleep between captures, in ms. Adaptive: fast under live-send, idle
    /// otherwise. Read by the worker thread each cycle.
    interval_ms: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Whether live-send is attached to this worker's pane. A failed capture means
    /// opposite things on the two sides: outside live mode it surfaces as an empty frame,
    /// during live-send the #1501 kill switch preserves the last-good frame.
    live: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Whether an empty capture is forwarded (clearing stale preview text) or dropped
    /// (the #1501 kill switch). Terminal / container panes forward so a cleared shell
    /// stops showing stale output; agent / tool panes preserve. Set per target by
    /// `set_forward_empty`.
    forward_empty: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Interrupts the inter-capture wait so a cadence or target change takes effect at
    /// once; without it, entering live-send mid-idle-sleep would lag the first fast
    /// capture by ~250ms.
    nudge: CaptureWake,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Newest OSC 52 clipboard write from the displayed pane. A VT grid extracts it from
    /// the live byte stream; terminal capture uses a separate raw observer, so rendered
    /// snapshots never carry the escape.
    clipboard: std::sync::Arc<std::sync::Mutex<Option<ClipboardFrame>>>,
    /// Whether the worker may render through a VT channel (`[tmux] vt_live`). Pushed by
    /// the render reconcile at spawn and on config refresh, and read each cycle, so
    /// toggling off tears down an armed channel and falls back in place.
    vt_enabled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Whether the raw OSC 52 observer may run when terminal rendering uses capture-pane.
    /// Mirrors the Clipboard Pass-through setting, so disabling it closes the second
    /// pipe-pane connection.
    clipboard_capture_enabled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Cycle counter bumped before every deadline-bounded sample. Changed frames cannot
    /// serve as a heartbeat because idle content is deduplicated, so render watches this
    /// counter and replaces a worker that stops advancing.
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
/// How often the worker re-asks how many panes its target window has and whether one is
/// zoomed. The answer changes only on a manual split, close or zoom, so a lazy cadence is
/// enough at one tiny `display-message` fork.
///
/// It also bounds a visible transient: the render-thread fallback probes `window_panes` in
/// the same fork as its capture, so it composites as soon as the user splits while the
/// worker keeps publishing single-pane frames until this elapses, and the preview
/// alternates until they agree.
const PANE_COUNT_PROBE_MS: u64 = 1_000;

/// How many panes the worker's target window has, for deciding whether the preview needs
/// the composite path. Returns 1 on any failure, keeping the caller on the cheap
/// single-pane transport.
///
/// A zoomed pane also reports 1: tmux keeps `window_panes` at the real count while
/// reporting every pane at the window's full rectangle, so the compositor's tiling
/// assumption breaks and compositing would hide the zoomed pane behind border fill.
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

/// Capture transport for a split window: every pane laid back out on the window grid, plus
/// pane 0's cursor. The cursor rides along because this path also serves live-send with no
/// VT channel, where dropping it would cost a split preview its painted cursor and the
/// alternate-screen and mouse-mode flags the wheel forward reads.
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
/// How long a cached window layout serves the composite before the panes around pane 0 are
/// re-captured. Only pane 0 takes input, so only its latency is felt: refreshing the others
/// at this cadence while pane 0 comes from its VT grid keeps echo latency identical to an
/// unsplit session at roughly three forks a second.
const COMPOSITE_LAYOUT_MS: u64 = 300;

/// Composite transport for a split window with a live VT channel on pane 0: pane 0's rows
/// come from the grid every call, every other pane from `cache`, re-captured on the
/// [`COMPOSITE_LAYOUT_MS`] cadence. Falls back to the all-panes fork when there is no
/// usable layout or the grid cannot be read.
///
/// `last_pane_probe` is reset whenever the layout capture fails, which is the signal that
/// `pane_count` is stale: the chained capture addresses `^.0..^.{pane_count-1}`, so a pane
/// the user closed makes tmux exit non-zero for the whole invocation. Without the reset the
/// count would stay wrong until [`PANE_COUNT_PROBE_MS`] elapsed and, since the cache is
/// stamped only on success, the failing fork would repeat every frame while the preview
/// composited a ghost of the closed pane.
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
            // The window is no longer the one these rectangles describe: drop them rather
            // than composite a pane that is gone, force a count re-probe, and let the
            // fallback carry this frame, since it probes `window_panes` in the same fork as
            // its capture.
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
    let Some(sample) =
        channel.sample_rows_padded_with_deadline(first.width, first.height, deadline)
    else {
        return capture_composited(name, lines, forward_empty, deadline);
    };
    if sample.incomplete {
        // Pane 0 is mid-repaint. Splicing it beside whole-captured panes tears the
        // composite, and a `capture-pane` fallback would fork to read the same half-drawn
        // cells, so keep the frame the preview has until the bracket closes.
        return (None, None);
    }
    let (rows, mut cursor) = (sample.rows, sample.cursor);

    // The sampled cursor stays pane relative after its rows are painted on the window
    // grid, so rebase only the frame dimensions and carry pane 0's rectangle for the
    // renderer to add its origin.
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
#[cfg(test)]
type TestCapture = Box<dyn FnMut() -> (Option<String>, Option<crate::tmux::PaneCursor>) + Send>;

impl LiveCaptureWorker {
    pub(in crate::tui) fn spawn(wake: std::sync::Arc<tokio::sync::Notify>) -> Self {
        Self::spawn_inner(
            wake,
            #[cfg(test)]
            None,
            #[cfg(test)]
            None,
        )
    }

    #[cfg(test)]
    pub(in crate::tui) fn spawn_with_capture_for_test(
        wake: std::sync::Arc<tokio::sync::Notify>,
        capture: impl FnMut() -> (Option<String>, Option<crate::tmux::PaneCursor>) + Send + 'static,
    ) -> (Self, std::sync::mpsc::Receiver<(u64, usize)>) {
        let (tx, rx) = std::sync::mpsc::channel();
        (
            Self::spawn_inner(wake, Some(Box::new(capture)), Some(tx)),
            rx,
        )
    }

    fn spawn_inner(
        wake: std::sync::Arc<tokio::sync::Notify>,
        #[cfg(test)] mut test_capture: Option<TestCapture>,
        #[cfg(test)] cycle_done: Option<std::sync::mpsc::Sender<(u64, usize)>>,
    ) -> Self {
        #[cfg(test)]
        let scripted_capture = test_capture.is_some();
        #[cfg(not(test))]
        let scripted_capture = false;
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
        // Spawned enabled; the render reconcile pushes the real `[tmux] vt_live` value
        // right after spawn and on every config refresh, so the worker never reads config.
        let vt_enabled = Arc::new(AtomicBool::new(true));
        // Start disabled until the render reconcile publishes the Clipboard Pass-through
        // setting, so a new worker never runs an observer before it is configured.
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
            // Budget the held capture was published at. A budget change alone (scroll
            // depth, viewport resize over a quiet pane) must republish even when the bytes
            // are identical, or consumers waiting for a deeper capture stall forever.
            let mut last_published_budget: usize = 0;
            let mut last_published_cursor: Option<crate::tmux::PaneCursor> = None;
            // When the frame that is currently in the mailbox (or the last
            // one consumed) was published, for the inter-publish floor.
            let mut last_published_at: Option<std::time::Instant> = None;
            // When the current unpublished change first entered the repaint-quiescence
            // debounce, for its latency cap. `None` whenever nothing is pending; cleared on
            // publish, on retarget, and when a cycle ends with nothing deferred.
            let mut pending_since: Option<std::time::Instant> = None;
            // Render the live preview from an in-process vt100 grid fed by `tmux pipe-pane`
            // (and, on tmux 3.8+, route input back through the same socket) instead of
            // scraping `capture-pane` and forking `send-keys` per keystroke. Default on;
            // `[tmux] vt_live` turns it off via `vt_enabled_cell`, re-read every cycle. A
            // pane that cannot arm a channel falls back to capture.
            #[cfg(unix)]
            let mut vt_source: Option<std::sync::Arc<crate::tmux::vt::VtChannel>> = None;
            // Terminal snapshots carry no raw OSC 52 escapes, so when VT is off for a
            // terminal pane this observer restores clipboard forwarding without building a
            // grid or seed.
            #[cfg(unix)]
            let mut osc52_source: Option<
                std::sync::Arc<crate::tmux::vt::Osc52Channel>,
            > = None;
            #[cfg(unix)]
            let mut osc52_seen = 0;
            // When the last arm attempt for this target ran, so a failure or a channel
            // death (the pane killed and its tmux session recreated under the same name)
            // retries on a throttle instead of re-arming every tick or latching onto the
            // capture fallback until the user switches panes. Reset on target change.
            #[cfg(unix)]
            let mut last_vt_arm: Option<std::time::Instant> = None;
            #[cfg(unix)]
            let mut last_osc52_arm: Option<std::time::Instant> = None;
            // Earliest arm for the current target (`channel_arm_due`), and the
            // arms in flight for it or an earlier target.
            #[cfg(unix)]
            let mut arm_after: Option<std::time::Instant> = None;
            #[cfg(unix)]
            let mut pending_vt_arm: Option<PendingArm<crate::tmux::vt::VtChannel>> = None;
            #[cfg(unix)]
            let mut pending_osc52_arm: Option<
                PendingArm<crate::tmux::vt::Osc52Channel>,
            > = None;
            // When the current target was first seen, until its first frame
            // publishes; traces the retarget-to-first-frame interval.
            let mut retarget_seen: Option<std::time::Instant> = None;
            // Panes in the target window, refreshed on the lazy `PANE_COUNT_PROBE_MS`
            // cadence. The seed only covers the gap before the first probe, which runs on
            // the first cycle, so an already-split window composites immediately. Reset on
            // retarget.
            let mut pane_count: u16 = 1;
            let mut last_pane_probe: Option<std::time::Instant> = None;
            // Window geometry plus every pane's captured rows, reused across frames while
            // pane 0 re-renders from its VT grid. Dropped on retarget and on any pane-count
            // change, since the cached rectangles then describe a layout that is gone.
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
                // How long a change deferred this iteration must wait before re-checking
                // (the sooner of the publish-floor and debounce remainders). `Some` shrinks
                // the wait below so the held frame goes out as its blockers reopen.
                let mut defer_wait_ms: Option<u64> = None;
                // The generation this cycle's frames belong to, read once per cycle so a
                // mid-fork retarget is caught by the still_current recheck and a frame that
                // slips past still carries the old generation for the consumer to drop.
                let generation_now = generation_cell.load(Ordering::Relaxed);
                let command_deadline = crate::tmux::TmuxCommandDeadline::new();
                // Channels detached this cycle, shut down after its frame.
                #[cfg(unix)]
                let mut stale_vt = Vec::new();
                #[cfg(unix)]
                let mut stale_osc52 = Vec::new();
                // A retarget resets the dedup so the new generation's first frame always
                // publishes, even in an A -> B -> A switch between cycles that leaves the
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
                    retarget_seen = Some(std::time::Instant::now());
                    // Detach the channels armed for the old target (also on
                    // retarget-to-empty); they shut down after this cycle's frame so the
                    // teardown fork never precedes the new pane's first frame. An arm still
                    // in flight finishes and is dropped on arrival by its generation.
                    #[cfg(unix)]
                    {
                        stale_vt.extend(vt_source.take());
                        last_vt_arm = None;
                        arm_after = Some(std::time::Instant::now() + CHANNEL_ARM_SETTLE);
                        next_authoritative_capture = None;
                        stale_osc52.extend(osc52_source.take());
                        osc52_seen = 0;
                        last_osc52_arm = None;
                    }
                    pane_count = 1;
                    last_pane_probe = None;
                    composite_layout = None;
                }
                // Adopt finished arms before the enable checks below so a
                // setting toggled off meanwhile tears the channel down at once.
                #[cfg(unix)]
                if let Some(v) =
                    PendingArm::take(&mut pending_vt_arm, generation_now, &mut stale_vt)
                {
                    // Event-driven echo: the channel pokes the nudge condvar on every grid
                    // change, so the wait below ends as output lands rather than after a
                    // poll interval.
                    v.set_change_wakeup(nudge_thread.clone());
                    next_authoritative_capture =
                        Some(std::time::Instant::now() + AUTHORITATIVE_CAPTURE_INTERVAL);
                    vt_source = Some(v);
                }
                #[cfg(unix)]
                if let Some(source) =
                    PendingArm::take(&mut pending_osc52_arm, generation_now, &mut stale_osc52)
                {
                    osc52_seen = source.clipboard_sequence();
                    osc52_source = Some(source);
                }
                // `[tmux] vt_live`, re-read every cycle. Toggling off tears down an armed
                // channel, and resetting the arm latch lets a later re-enable arm afresh for
                // the same target instead of waiting for a retarget.
                #[cfg(unix)]
                let vt_enabled = !scripted_capture && vt_enabled_cell.load(Ordering::Relaxed);
                #[cfg(unix)]
                let clipboard_capture_enabled =
                    !scripted_capture && clipboard_capture_enabled_cell.load(Ordering::Relaxed);
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
                    // Keep a lazy count of the target window's panes so a hand-made split
                    // stops being invisible: one tiny fork every couple of seconds.
                    if !scripted_capture
                        && last_pane_probe.is_none_or(|t| {
                            t.elapsed() >= std::time::Duration::from_millis(PANE_COUNT_PROBE_MS)
                        })
                    {
                        last_pane_probe = Some(std::time::Instant::now());
                        let seen = probe_pane_count(&name, &command_deadline);
                        if seen != pane_count {
                            // Layout changed under us; the cached rectangles no
                            // longer describe this window.
                            composite_layout = None;
                            pane_count = seen;
                        }
                    }
                    // A split window renders through the compositor in both passive and
                    // live mode. With a VT channel armed it costs no more per frame than an
                    // unsplit session: pane 0 still comes from the grid, and only the panes
                    // beside it are re-captured, on their own cadence.
                    let composite = pane_count > 1;
                    // An OSC 52 clipboard write the displayed pane emitted since the last
                    // cycle, published below under the same retarget guard as the cursor.
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
                        && channel_arm_due(
                            arm_after,
                            osc52_source.is_some() || pending_osc52_arm.is_some(),
                            last_osc52_arm,
                            std::time::Instant::now(),
                        )
                    {
                        last_osc52_arm = Some(std::time::Instant::now());
                        pending_osc52_arm = Some(PendingArm::spawn(
                            name.clone(),
                            generation_now,
                            nudge_thread.clone(),
                            crate::tmux::vt::Osc52Channel::acquire_with_deadline,
                        ));
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
                    // Acquire one frame plus cursor. By default sample the in-process
                    // vt100 grid, arming a `pipe-pane` channel once the selection rests on
                    // this target (cursor and alt/mouse flags come authoritatively from the
                    // grid). Until armed, or if arming fails, one `capture-pane` fork serves
                    // the pane and retries on the `VT_REARM_INTERVAL` throttle.
                    // Capture-path policy: outside live-send an empty capture is forwarded,
                    // so a killed pane surfaces as "No output available" rather than stale
                    // bytes; during live-send the #1501 kill switch preserves the last-good
                    // frame against transient tmux errors.
                    let forward_empty = forward_empty_policy || !live_cell.load(Ordering::Relaxed);
                    #[cfg(test)]
                    let capture_override = test_capture.as_mut().map(|capture| capture());
                    #[cfg(not(test))]
                    let capture_override: Option<(
                        Option<String>,
                        Option<crate::tmux::PaneCursor>,
                    )> = None;
                    #[cfg(unix)]
                    let (capture, cursor_now) = if let Some(capture) = capture_override {
                        capture
                    } else if vt_enabled {
                        if channel_arm_due(
                            arm_after,
                            vt_source.is_some() || pending_vt_arm.is_some(),
                            last_vt_arm,
                            std::time::Instant::now(),
                        ) {
                            last_vt_arm = Some(std::time::Instant::now());
                            pending_vt_arm = Some(PendingArm::spawn(
                                name.clone(),
                                generation_now,
                                nudge_thread.clone(),
                                crate::tmux::vt::VtChannel::acquire_with_deadline,
                            ));
                        }
                        // A channel whose forwarder disconnected stops updating its grid,
                        // so drop it and fall back to capture for this pane; the throttle
                        // re-arms after `VT_REARM_INTERVAL` (a session restart reuses the
                        // name) without thrashing on a permanently broken pane.
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
                                    let sample = v.sample_with_deadline(lines, &command_deadline);
                                    // A half-drawn synchronized-output frame is not
                                    // published: the bracket closing brings a whole one and
                                    // the preview keeps the last frame until then.
                                    if sample.incomplete {
                                        (None, None)
                                    } else {
                                        (Some(sample.content), sample.cursor)
                                    }
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
                    let (capture, cursor_now) = if let Some(capture) = capture_override {
                        capture
                    } else if composite {
                        capture_composited(&name, lines, forward_empty, &command_deadline)
                    } else {
                        capture_via_tmux(&name, lines, forward_empty, &command_deadline)
                    };
                    // Chunk-arrival timing for the repaint-quiescence debounce, only while
                    // sampling a live VT grid; `None` on the capture fallback and non-unix,
                    // which leaves pacing to the publish floor.
                    #[cfg(unix)]
                    let vt_timing = vt_source.as_ref().and_then(|v| v.chunk_timing());
                    #[cfg(not(unix))]
                    let vt_timing: Option<(u64, u64)> = None;
                    // Recheck the target once for both publishes: a retarget mid-fork means
                    // these bytes belong to the old pane. `set_target` also clears the
                    // mailbox, but the fork may have started before that switch.
                    let still_current = target_cell.lock().ok().is_some_and(|g| *g == name)
                        && generation_cell.load(Ordering::Relaxed) == generation_now;
                    // Publish an agent clipboard write and wake the render loop even if the
                    // frame dedups, fails or is withheld as half drawn: the read above
                    // already consumed it, and this tap is the agent's only path to the host
                    // clipboard (#2420).
                    if still_current {
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
                    }
                    if let Some(content) = capture {
                        // Skip unchanged frames, and empties unless this pane forwards
                        // them, so only changed captures wake the render loop and an idle
                        // pane never repaints.
                        let changed = last_captured.as_deref() != Some(content.as_str());

                        if still_current {
                            // A budget change alone must republish even when the bytes are
                            // identical, or consumers waiting for a deeper capture stall.
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
                                // Repaint-quiescence debounce (VT path only): hold a
                                // changed frame while output streams back-to-back so a
                                // multi-chunk clear-then-reprint publishes once settled
                                // (#2903). A lone chunk reports not-streaming and never
                                // waits, preserving the #2822 echo path.
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
                                    if let Some(seen) = retarget_seen.take() {
                                        tracing::debug!(
                                            target: "tui.live_send",
                                            pane = %name,
                                            elapsed_ms = seen.elapsed().as_millis() as u64,
                                            "preview: first frame after retarget"
                                        );
                                    }
                                    wake.notify_one();
                                } else {
                                    // Held by the floor or the debounce: defer, don't
                                    // drop. `last_captured` stays stale so the next cycle
                                    // re-detects this frame and publishes it.
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
                #[cfg(unix)]
                {
                    for channel in stale_vt.drain(..) {
                        shutdown_vt_source(&mut Some(channel), &command_deadline);
                    }
                    for channel in stale_osc52.drain(..) {
                        shutdown_osc52_source(&mut Some(channel), &command_deadline);
                    }
                }
                // Nothing deferred means no frame is held, so clear the debounce hold and
                // let the next repaint's latency cap start fresh. Covers a mid-repaint
                // change that evaporated back to the published content without publishing,
                // which would leave a stale `pending_since` that makes the next repaint hit
                // the cap immediately and skip the debounce.
                if defer_wait_ms.is_none() {
                    pending_since = None;
                }
                #[cfg(test)]
                if let Some(done) = &cycle_done {
                    if !name.is_empty()
                        && generation_cell.load(Ordering::Relaxed) == generation_now
                        && target_cell.lock().is_ok_and(|target| *target == name)
                    {
                        let _ = done.send((generation_now, lines));
                    }
                }
                // Interruptible wait: `set_live` / `set_target` notify the condvar so a
                // cadence or target change is picked up at once, and on the VT path the
                // channel's reader notifies on every grid change, so fresh output samples
                // immediately. A generation under the condvar mutex preserves a wake that
                // arrives before this thread parks; coalesced wakes cost one extra cycle,
                // which dedup makes harmless.
                //
                // A live vt channel samples the grid cheaply and dedups unchanged frames, so
                // the idle throttle buys nothing there: pace it fast so the previewed pane
                // streams as smoothly as the live one. The idle cadence still governs the
                // capture-pane fallback, where every sample is a fork.
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
                // Start the settled arm on time rather than after a full idle
                // interval, while no channel of either kind is held or pending.
                #[cfg(unix)]
                let ms = match arm_after.filter(|_| {
                    vt_source.is_none()
                        && pending_vt_arm.is_none()
                        && osc52_source.is_none()
                        && pending_osc52_arm.is_none()
                }) {
                    Some(t) => {
                        let remaining = t.saturating_duration_since(std::time::Instant::now());
                        if remaining.is_zero() {
                            ms
                        } else {
                            ms.min(remaining.as_millis() as u64).max(1)
                        }
                    }
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

    /// Push the `[tmux] vt_live` setting into the worker: one atomic store, called after
    /// spawn and on every config refresh, so a toggle applies on the next cycle without a
    /// respawn. The nudge runs that cycle now, since a disable mid-idle-sleep would
    /// otherwise keep the armed channel for up to 250ms.
    pub(in crate::tui) fn set_vt_enabled(&self, enabled: bool) {
        let prev = self
            .vt_enabled
            .swap(enabled, std::sync::atomic::Ordering::Relaxed);
        if prev != enabled {
            self.nudge();
        }
    }

    /// Enable raw OSC 52 observation for terminal previews rendered via capture-pane. A VT
    /// grid is unaffected: it already extracts clipboard writes from its own pipe.
    pub(in crate::tui) fn set_clipboard_capture_enabled(&self, enabled: bool) {
        let prev = self
            .clipboard_capture_enabled
            .swap(enabled, std::sync::atomic::Ordering::Relaxed);
        if prev != enabled {
            self.nudge();
        }
    }

    /// A cloneable handle the send worker uses to nudge this worker after each dispatched
    /// batch (echo latency). Backed by the same condvar as `set_live` / `set_target`, so a
    /// wake just runs one capture cycle early.
    pub(in crate::tui) fn waker(&self) -> LiveCaptureWake {
        LiveCaptureWake {
            nudge: self.nudge.clone(),
        }
    }

    /// Choose whether empty captures clear the preview (terminal / container panes) or
    /// preserve the last-good frame (agent / tool panes, the #1501 kill switch). One atomic
    /// store, called from the render reconcile alongside `set_target`.
    pub(in crate::tui) fn set_forward_empty(&self, forward: bool) {
        self.forward_empty
            .store(forward, std::sync::atomic::Ordering::Relaxed);
    }

    /// Current empty-frame policy, re-read by the render consumer after a potentially
    /// blocking capture. Terminal and container panes always clear; agent and tool panes
    /// clear only outside live-send, preserving #1501 across a racing live-mode transition.
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

    /// Point the worker at a different pane (its tmux session name; empty to idle). Cheap:
    /// swaps the shared name and drops any capture queued from the previous pane, so the
    /// render never applies stale bytes under the new view, and never blocks on a fork.
    pub(in crate::tui) fn set_target(&self, name: String) {
        let changed = if let Ok(mut guard) = self.target.lock() {
            if *guard != name {
                *guard = name;
                // Invalidate every frame the in-flight cycle might publish: it tagged them
                // with the old generation, so even one that slips past the mailbox clear is
                // dropped by `frame_is_current`.
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

    /// Switch the capture cadence between live-send (fast) and background preview (idle).
    /// One atomic store from the render reconcile, so entering or leaving live mode retunes
    /// the worker in place. Does not touch the cursor: it is published every cycle for the
    /// passive wheel forward, and the render paints it only under live-send.
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
            // Apply the new cadence now: a mid-idle-sleep worker would keep the old
            // interval for a cycle and lag the first live capture on entry.
            self.nudge();
        }
    }

    /// Publish the line count the worker should capture: one atomic store per render, so
    /// resizes and history scroll reach the worker promptly.
    pub(in crate::tui) fn set_capture_lines(&self, lines: usize) {
        self.capture_lines
            .store(lines, std::sync::atomic::Ordering::Relaxed);
    }

    /// Put a frame a consumer rejected back into the mailbox so it survives until one can
    /// apply it: the worker's dedup is content-based, so a consumed-and-dropped frame (a
    /// terminal-clear empty landing while the preview is frozen) would never be republished.
    /// Never overwrites a newer frame.
    pub(in crate::tui) fn restore_latest(&self, frame: CaptureFrame) {
        if let Ok(mut guard) = self.latest.lock() {
            if guard.is_none() {
                *guard = Some(frame);
            }
        }
    }

    /// Take the newest frame since the last call, or `None` when nothing new arrived, in
    /// which case the render loop keeps the current preview.
    pub(in crate::tui) fn take_latest(&self) -> Option<CaptureFrame> {
        self.latest.lock().ok().and_then(|mut guard| guard.take())
    }

    /// Snapshot of the worker's cycle counter for render-side stall detection. Publication
    /// is not used, because an unchanged pane publishes nothing while the worker is healthy.
    pub(in crate::tui) fn cycles(&self) -> u64 {
        self.cycles.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Whether `frame` belongs to the worker's current target generation. A frame captured
    /// before the last set_target must be dropped, never applied or restored.
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

/// Walk one drained batch and execute it as one-shot tmux subprocesses: coalescing merges
/// literal runs into one send-keys call, while named keys and resizes dispatch singly.
fn dispatch_batch(
    tmux_name: &str,
    resize_owner: &str,
    batch: Vec<WorkerMsg>,
) -> ResizeDispatchResult {
    let actions = coalesce(batch);
    // A Paste can only go through tmux (`paste-buffer -p` decides whether the pane gets
    // bracketed-paste markers), so pin the whole mixed batch to tmux and keep one ordered
    // writer for the pty.
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

/// Execute one TmuxAction as a one-shot tmux subprocess. A module-level fn rather than a
/// method, so the spawned thread can call it without holding a worker reference.
fn dispatch_via_fork(
    tmux_name: &str,
    action: &TmuxAction,
    force_tmux: bool,
    resize_owner: Option<&str>,
) -> anyhow::Result<()> {
    use std::process::Stdio;

    // Fast path (`[tmux] vt_live`): while a live input channel is armed for this pane, all
    // pane input goes through the socket and none through `send-keys`. Mixing the two would
    // interleave writers on one pty input stream and can corrupt multi-byte sequences, as
    // `pipe-pane -I` arbitrates nothing. `input_mode` returns `Some` only while the
    // forwarder is connected, so a dead or not-yet-connected channel falls through to the
    // fork below instead of vanishing. Keys are encoded here against the pane's DECCKM from
    // the grid, since tmux's own translation is bypassed. `Resize` is not pane input.
    //
    // The invariant forbids concurrent writers, not a sequential fallback:
    // `try_send_input`'s `write_all` on a blocking `UnixStream` fails only on a broken pipe,
    // never a transient WouldBlock, so `false` means the forwarder died between the
    // `input_mode` check and this write, leaving no live writer and making the fork safe. An
    // empty-bytes encoding still drops without forking: nothing proves the writer is dead,
    // so falling back could race a live socket writer.
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
            // tmux's command parser reads a trailing `;` in a `send-keys -l` payload as a
            // command separator and drops it, even after `--`, so it never reaches the pane
            // (#1942). Peel the trailing semicolons and send them as raw hex (`-H 3b`),
            // which tmux passes through verbatim; embedded and leading ones survive `-l`.
            let (head, semis) = peel_trailing_semicolons(s);
            if semis > 0 {
                if !head.is_empty() {
                    send_literal(&target, head)?;
                }
                return crate::tmux::Session::from_name(tmux_name)
                    .send_raw_bytes(&vec![0x3b; semis]);
            }
            // `-l --` mirrors `send_literal_no_enter`: a literal send plus the
            // end-of-options marker, so a payload starting with `-` isn't read as a flag.
            cmd.args(["send-keys", "-t", &target, "-l", "--", s.as_str()]);
        }
        TmuxAction::Named(name) => {
            cmd.args(["send-keys", "-t", &target, name.as_str()]);
        }
        TmuxAction::NamedRepeat { name, count } => {
            // `-N <count>` repeats the key in one fork. tmux renders each press in the
            // pane's current cursor-key mode, so wheel-forward arrows honor DECCKM.
            let count = count.to_string();
            cmd.args(["send-keys", "-t", &target, "-N", &count, name.as_str()]);
        }
        TmuxAction::HexBytes(bytes) => {
            // `-H` sends each arg as the hex value of an ASCII character, used for control
            // bytes (CR, TAB, ESC) and the bracketed-paste markers, none of which ride a
            // `-l` payload safely. ARG_MAX chunking and the hex encoding live in the shared
            // tmux layer, which the web live view's input path also uses.
            return crate::tmux::Session::from_name(tmux_name).send_raw_bytes(bytes);
        }
        TmuxAction::Paste(text) => {
            // tmux emits the bracketed-paste markers only when the program set DECSET
            // 2004, so a raw shell gets clean text instead of literal `00~` / `01~`.
            return crate::tmux::Session::from_name(tmux_name).paste_text(text);
        }
        TmuxAction::Resize { cols, rows } => {
            // tmux checks ownership in the same command queue as the resize; the worker's
            // earlier heartbeat check only filters stale batches and cannot authorize a
            // later subprocess.
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

/// Encode a `TmuxAction` to raw terminal bytes for the persistent-input fast path. It
/// bypasses tmux's `send-keys` translation, so that translation is reproduced here,
/// honoring the pane's DECCKM (`app_cursor`) for arrows and nav keys. An empty vec means a
/// key that cannot be encoded, dropped under the single-writer rule. `Resize` never
/// reaches here.
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

/// Encode one tmux key name (`Up`, `C-c`, `S-Up`, `M-x`, `F5`) to terminal bytes. Cursor
/// and nav keys honor `app_cursor` and the xterm modifier parameter
/// (`1 + shift + alt*2 + ctrl*4`). Empty vec means unencodable.
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

    // Editing block (CSI n ~), modifier as `;modp`, unaffected by DECCKM.
    // `PageUp`/`PageDown` are accepted alongside tmux's `PPage`/`NPage` because the wheel
    // and edge-autoscroll paths emit the former; without the alias those keys would encode
    // to nothing on the VT input path.
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
            // Single char: `C-<letter>` becomes a C0 control byte, otherwise the char,
            // ESC-prefixed for Alt. Shift never reaches here, since plain chars are sent
            // literally with case applied.
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
    fn keys_and_actions_encode_to_vt_bytes() {
        // Only unmodified cursor keys follow DECCKM; modified keys use the CSI modifier
        // param (1 + shift + 2*alt + 4*ctrl). PageUp/PageDown alias PPage/NPage because the
        // page-forward paths emit those names.
        let cases: &[(&str, bool, &[u8])] = &[
            ("Up", false, b"\x1b[A"),
            ("Up", true, b"\x1bOA"),
            ("Down", false, b"\x1b[B"),
            ("Right", true, b"\x1bOC"),
            ("Left", false, b"\x1b[D"),
            ("Home", true, b"\x1bOH"),
            ("End", false, b"\x1b[F"),
            ("S-Up", false, b"\x1b[1;2A"),
            ("S-Up", true, b"\x1b[1;2A"),
            ("C-Up", false, b"\x1b[1;5A"),
            ("M-Up", false, b"\x1b[1;3A"),
            ("C-S-Left", false, b"\x1b[1;6D"),
            ("PPage", false, b"\x1b[5~"),
            ("NPage", true, b"\x1b[6~"),
            ("PageUp", false, b"\x1b[5~"),
            ("PageDown", false, b"\x1b[6~"),
            ("C-PageUp", false, b"\x1b[5;5~"),
            ("DC", false, b"\x1b[3~"),
            ("S-DC", false, b"\x1b[3;2~"),
            ("F1", false, b"\x1bOP"),
            ("F4", false, b"\x1bOS"),
            ("F5", false, b"\x1b[15~"),
            ("F12", false, b"\x1b[24~"),
            ("C-F5", false, b"\x1b[15;5~"),
            ("Enter", false, b"\r"),
            ("Tab", false, b"\t"),
            ("BTab", false, b"\x1b[Z"),
            ("BSpace", false, b"\x7f"),
            ("Escape", false, b"\x1b"),
            ("Space", false, b" "),
            ("C-Space", false, b"\x00"),
            ("C-c", false, b"\x03"),
            ("C-a", false, b"\x01"),
            ("M-x", false, b"\x1bx"),
            ("M-Enter", false, b"\x1b\r"),
        ];
        for &(name, app_cursor, expected) in cases {
            assert_eq!(
                enc(name, app_cursor),
                expected,
                "{name} app_cursor={app_cursor}"
            );
        }

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

/// Cap on concurrently in-flight passive-preview send forks. A fast wheel flick fires many
/// notches, and without a ceiling each would spawn its own detached thread. Eight keeps
/// scroll responsive, and dropping a notch past that is harmless.
const MAX_INFLIGHT_ONESHOT: usize = 8;
static INFLIGHT_ONESHOT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Releases one `INFLIGHT_ONESHOT` slot on drop, so the count balances even if the fork
/// thread panics. Constructed inside the spawned closure, so a spawn that never starts must
/// release its reserved slot itself.
struct OneshotSlot;
impl Drop for OneshotSlot {
    fn drop(&mut self) {
        INFLIGHT_ONESHOT.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

/// Forward a single translated key to a tmux pane with a one-shot `tmux send-keys` fork on
/// a detached thread, for the passive-preview wheel forward where there is no
/// `LiveSendWorker` to enqueue onto and `dispatch_via_fork` would block the UI thread.
/// Fire-and-forget: a dropped notch is harmless and a failed fork is logged. Scroll notches
/// carry no ordering relationship, so racing forks are fine within
/// `MAX_INFLIGHT_ONESHOT`.
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
    // `Builder::spawn` returns the OS error instead of panicking, so a thread-creation
    // failure under load can't take down the calling UI thread. The `OneshotSlot` guard
    // inside the closure releases the slot on completion or panic; a spawn that never
    // starts releases it here.
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

/// Upper bound on bytes encoded into one `tmux send-keys -H` fork. Each byte becomes a
/// ~2-char hex argument plus its argv pointer (~11 bytes of kernel arg space) and macOS caps
/// `execve` argv+envp at 256 KiB, so a large paste overflows around 20 KB and fails with
/// E2BIG. 4 KiB per fork keeps every argv under ~45 KiB while keeping the fork count low.
/// Split a literal payload into its leading content and the count of trailing `;` bytes.
/// tmux drops a trailing `;` from a `send-keys -l` payload, reading it as a command
/// separator even after `--` (#1942), so the caller sends `head` literally and the peeled
/// semicolons as raw hex. Embedded and leading semicolons survive untouched.
fn peel_trailing_semicolons(s: &str) -> (&str, usize) {
    let head = s.trim_end_matches(';');
    (head, s.len() - head.len())
}

/// Send a literal string through one `tmux send-keys -l --` fork, for the head of a payload
/// whose trailing semicolons were peeled off (see [`dispatch_via_fork`]).
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

/// What the translator says to do with one incoming key event. The exit-chord check lives
/// in `handle_live_send_key`, which consults the user's configured chord list, so translate
/// is purely the key-to-tmux mapping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum LiveDispatch {
    /// Forward the keystroke to tmux in the requested form.
    Send(TmuxKey),
    /// Key has no meaningful tmux mapping (Null, CapsLock, media keys, …).
    /// Caller should drop it silently rather than echo it elsewhere.
    Ignore,
}

/// How the translator wants the keystroke delivered: `Literal` through
/// `tmux send-keys -l --`, named keys through `tmux send-keys`, `NamedRepeat` through
/// `send-keys -N <count>` (one fork for N presses), and `HexBytes` through `send-keys -H`
/// for raw bytes that cannot ride a literal payload (ESC, CR, TAB, paste markers).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum TmuxKey {
    Literal(String),
    Named(String),
    /// A named key sent `count` times in one fork, so the wheel forward delivers a notch's
    /// arrow presses without one fork each.
    NamedRepeat {
        name: String,
        count: usize,
    },
    HexBytes(Vec<u8>),
    /// A multi-line paste delivered through tmux's `paste-buffer -p`, so tmux decides
    /// whether the program gets bracketed-paste markers. Never merged with neighbours.
    Paste(String),
}

/// Map one crossterm `KeyEvent` onto a `LiveDispatch`. Exit-chord detection happens in
/// `handle_live_send_key` before this is called, so this is pure key-to-tmux mapping.
///
/// Conventions:
/// - Plain printable chars go literal, preserving case and punctuation; Shift is implicit
///   in the char, so no `S-` is added.
/// - Ctrl/Alt plus a char folds to lowercase and emits a tmux name (`C-a`, `M-x`, `C-M-x`),
///   the conventional form for tmux's case-insensitive chord names.
/// - Named keys include `S-` when Shift is held, so editors see `S-Up` for shift-arrow
///   selection. `BackTab` is the exception: the keycode already means Shift+Tab, so it
///   emits `BTab`.
pub fn translate(key: KeyEvent) -> LiveDispatch {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    // Shift+Enter alone becomes ESC+CR, the readline "meta-Enter inserts a newline"
    // convention and byte-identical to tmux's `M-Enter`, so agents that accept Alt+Enter
    // need no further mapping. Only reachable under DISAMBIGUATE_ESCAPE_CODES on a
    // kitty-protocol terminal (#2362); legacy terminals deliver bare Enter and fall through
    // to the named-key path. Strict modifier equality keeps Ctrl+Shift+Enter and
    // Alt+Shift+Enter on that path for future keybinds. HexBytes short-circuits tmux
    // chord-name parsing and matches the byte representation across tmux versions.
    if key.code == KeyCode::Enter && key.modifiers == KeyModifiers::SHIFT {
        return LiveDispatch::Send(TmuxKey::HexBytes(vec![0x1b, b'\r']));
    }

    // Char path: tmux chord names are case-insensitive for letters and `Char(c)` already
    // carries Shift, so `S-` is dropped to avoid double-encoding.
    if let KeyCode::Char(c) = key.code {
        if ctrl || alt {
            let p = mod_prefix(ctrl, alt, false);
            return LiveDispatch::Send(TmuxKey::Named(format!("{p}{}", c.to_ascii_lowercase())));
        }
        return LiveDispatch::Send(TmuxKey::Literal(c.to_string()));
    }

    // Named-key path: Shift is meaningful (S-Up vs Up for editor selection). BackTab is
    // Shift+Tab by its own keycode, so it gets the no-shift prefix.
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

    fn wait_for_capture_cycle(
        worker: &LiveCaptureWorker,
        done: &std::sync::mpsc::Receiver<(u64, usize)>,
        lines: usize,
    ) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let cycle = done
                .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
                .expect("capture cycle completed");
            if cycle == (worker.current_generation_for_test(), lines) {
                return;
            }
        }
    }

    // translate never emits Exit (the chord check lives in handle_live_send_key); the
    // chord-list tests below cover the configurable exit path.

    #[test]
    fn parse_chord_accepts_known_forms_and_rejects_garbage() {
        let ctrl = KeyModifiers::CONTROL;
        let cases = [
            ("C-q", Some((KeyCode::Char('q'), ctrl))),
            // Uppercase folds to lowercase under Ctrl (tmux: C-a and C-A are one chord).
            ("C-Q", Some((KeyCode::Char('q'), ctrl))),
            ("C-]", Some((KeyCode::Char(']'), ctrl))),
            (
                "Ctrl+Alt+x",
                Some((KeyCode::Char('x'), ctrl | KeyModifiers::ALT)),
            ),
            ("F12", Some((KeyCode::F(12), KeyModifiers::NONE))),
            ("S-Up", Some((KeyCode::Up, KeyModifiers::SHIFT))),
            ("Escape", Some((KeyCode::Esc, KeyModifiers::NONE))),
            ("PageUp", Some((KeyCode::PageUp, KeyModifiers::NONE))),
            ("", None),
            ("X-q", None),  // unknown modifier
            ("C-qq", None), // multi-char key without F-prefix
            ("C-", None),   // missing key
        ];
        for (input, expected) in cases {
            assert_eq!(parse_chord(input), expected, "{input:?}");
        }
    }

    #[test]
    fn chord_matching_is_exact_with_ctrl_case_folding() {
        let ctrl_q = parse_chord("C-q").unwrap();
        let leader = parse_chord(DEFAULT_LEADER).unwrap();
        let cases = [
            // Crossterm may deliver Ctrl+Q as Char('q') or Char('Q')+SHIFT; the shift form
            // means the user wants to send Ctrl+Shift+q to the agent.
            (ctrl_q, KeyCode::Char('q'), KeyModifiers::CONTROL, true),
            (
                ctrl_q,
                KeyCode::Char('Q'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
                false,
            ),
            (
                ctrl_q,
                KeyCode::Char('q'),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
                false,
            ),
            // The armed leader fires again on Ctrl+B; bare `b` is a menu command.
            (leader, KeyCode::Char('b'), KeyModifiers::CONTROL, true),
            (leader, KeyCode::Char('b'), KeyModifiers::NONE, false),
        ];
        for (chord, code, mods, expected) in cases {
            assert_eq!(
                chord_matches(chord, KeyEvent::new(code, mods)),
                expected,
                "{chord:?} vs {code:?}+{mods:?}"
            );
        }
    }

    #[test]
    fn chord_list_parses_matches_and_displays() {
        let chords = parse_chord_list("C-q, garbage, C-]");
        assert_eq!(
            chords,
            vec![
                (KeyCode::Char('q'), KeyModifiers::CONTROL),
                (KeyCode::Char(']'), KeyModifiers::CONTROL),
            ]
        );
        for (c, expected) in [('q', true), (']', true), ('x', false)] {
            assert_eq!(
                chord_list_matches(
                    &chords,
                    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
                ),
                expected,
                "Ctrl+{c}"
            );
        }
        assert_eq!(display_chord_list(&chords), "Ctrl+Q / Ctrl+]");
        for (chord, shown) in [("F12", "F12"), ("Ctrl+Alt+Shift+x", "Ctrl+Alt+Shift+X")] {
            assert_eq!(display_chord(parse_chord(chord).unwrap()), shown);
        }

        // An all-invalid list must not trap the user in live mode with no exit.
        let fallback = parse_chord_list("not-a-chord, also-bad");
        assert!(!fallback.is_empty());
        assert_eq!(fallback, parse_chord_list(DEFAULT_EXIT_CHORD));
    }

    #[test]
    fn translate_maps_keys_to_tmux() {
        let lit = |s: &str| LiveDispatch::Send(TmuxKey::Literal(s.into()));
        let named = |s: &str| LiveDispatch::Send(TmuxKey::Named(s.into()));
        let none = KeyModifiers::NONE;
        let ctrl = KeyModifiers::CONTROL;
        let cases = [
            // translate is pure key->tmux: Ctrl+q passes through, and the exit decision
            // belongs to the chord-list matcher.
            (KeyCode::Char('q'), ctrl, named("C-q")),
            (KeyCode::Char('a'), none, lit("a")),
            (KeyCode::Char('Z'), none, lit("Z")),
            (KeyCode::Char('!'), none, lit("!")),
            (KeyCode::Char(' '), none, lit(" ")),
            (KeyCode::Char('c'), ctrl, named("C-c")),
            (KeyCode::Char('A'), ctrl, named("C-a")),
            (KeyCode::Char('x'), KeyModifiers::ALT, named("M-x")),
            (KeyCode::Char('q'), ctrl | KeyModifiers::ALT, named("C-M-q")),
            // The case carries Shift: Shift+A sends literal "A", not "S-a".
            (KeyCode::Char('A'), KeyModifiers::SHIFT, lit("A")),
            // Some terminals set SHIFT on BackTab too; tmux would reject "S-BTab".
            (KeyCode::BackTab, KeyModifiers::SHIFT, named("BTab")),
            (KeyCode::Esc, none, named("Escape")),
            (KeyCode::Enter, none, named("Enter")),
            (KeyCode::Tab, none, named("Tab")),
            (KeyCode::BackTab, none, named("BTab")),
            (KeyCode::Backspace, none, named("BSpace")),
            (KeyCode::Delete, none, named("DC")),
            (KeyCode::Insert, none, named("IC")),
            (KeyCode::Home, none, named("Home")),
            (KeyCode::End, none, named("End")),
            (KeyCode::PageUp, none, named("PPage")),
            (KeyCode::PageDown, none, named("NPage")),
            (KeyCode::F(1), none, named("F1")),
            (KeyCode::F(12), none, named("F12")),
            (KeyCode::F(5), ctrl, named("C-F5")),
        ];
        for (code, mods, expected) in cases {
            assert_eq!(translate(k_mod(code, mods)), expected, "{code:?}+{mods:?}");
        }
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
        // Editors rely on `S-Up` / `S-Down` for text selection: without the prefix
        // Shift+arrow looks like a plain arrow and the editor never sees the modifier.
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
        // Shift+Enter on a kitty-protocol terminal lands here (#2362). The agent reads
        // ESC+CR as readline's meta-Enter newline, identical to Alt+Enter on terminals that
        // pre-encode Shift+Enter that way.
        assert_hex(
            translate(k_mod(KeyCode::Enter, KeyModifiers::SHIFT)),
            b"\x1b\r",
        );
    }

    #[test]
    fn alt_enter_still_named_m_enter() {
        // Alt+Enter (terminals that pre-encode Shift+Enter as ESC+CR deliver Enter+ALT)
        // must keep producing the named `M-Enter`, which tmux expands to ESC+CR; the
        // kitty-protocol path must not displace it.
        assert_named(
            translate(k_mod(KeyCode::Enter, KeyModifiers::ALT)),
            "M-Enter",
        );
    }

    #[test]
    fn ctrl_shift_enter_falls_through_to_named() {
        // Strict modifier equality keeps C-S-Enter off the HexBytes arm and on the
        // named-key path, so a future keybind can target it distinctly.
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
        // Symmetric to `ctrl_shift_enter_falls_through_to_named`: any modifier beyond
        // SHIFT alone falls through to the named-key path, so `M-S-Enter` stays targetable.
        assert_named(
            translate(k_mod(
                KeyCode::Enter,
                KeyModifiers::ALT | KeyModifiers::SHIFT,
            )),
            "M-S-Enter",
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

    fn snd_named_repeat(name: &str, count: usize) -> WorkerMsg {
        WorkerMsg::Send(TmuxKey::NamedRepeat {
            name: name.into(),
            count,
        })
    }

    #[test]
    fn coalesce_merges_runs_and_preserves_order() {
        let lit = |s: &str| TmuxAction::Literal(s.into());
        let named = |s: &str| TmuxAction::Named(s.into());
        let rep = |s: &str, count| TmuxAction::NamedRepeat {
            name: s.into(),
            count,
        };
        let hex = |bytes: &[u8]| TmuxAction::HexBytes(bytes.to_vec());
        let paste_start = [0x1b, b'[', b'2', b'0', b'0', b'~'];
        let paste_end = [0x1b, b'[', b'2', b'0', b'1', b'~'];
        let cases: Vec<(&str, Vec<WorkerMsg>, Vec<TmuxAction>)> = vec![
            ("empty", vec![], vec![]),
            ("single literal", vec![snd_lit("a")], vec![lit("a")]),
            (
                "single named",
                vec![snd_named("Escape")],
                vec![named("Escape")],
            ),
            // Typing "hello" is one tmux send-keys call, not five.
            (
                "literal run",
                vec![
                    snd_lit("h"),
                    snd_lit("e"),
                    snd_lit("l"),
                    snd_lit("l"),
                    snd_lit("o"),
                ],
                vec![lit("hello")],
            ),
            // A named key mid-typing splits the run so it arrives in order.
            (
                "named splits run",
                vec![
                    snd_lit("a"),
                    snd_lit("b"),
                    snd_named("Up"),
                    snd_lit("c"),
                    snd_lit("d"),
                ],
                vec![lit("ab"), named("Up"), lit("cd")],
            ),
            // tmux send-keys won't accept two named keys as one literal.
            (
                "back-to-back named",
                vec![snd_named("Up"), snd_named("Up")],
                vec![named("Up"), named("Up")],
            ),
            // Wheel notches drained in one batch collapse to one `send-keys -N` fork.
            (
                "same repeats fold",
                vec![snd_named_repeat("Up", 3), snd_named_repeat("Up", 3)],
                vec![rep("Up", 6)],
            ),
            (
                "direction change keeps both repeats",
                vec![snd_named_repeat("Up", 3), snd_named_repeat("Down", 3)],
                vec![rep("Up", 3), rep("Down", 3)],
            ),
            (
                "repeat flushes literal run",
                vec![snd_lit("ab"), snd_named_repeat("Down", 3), snd_lit("cd")],
                vec![lit("ab"), rep("Down", 3), lit("cd")],
            ),
            (
                "trailing literal flushed",
                vec![snd_named("Tab"), snd_lit("x"), snd_lit("y")],
                vec![named("Tab"), lit("xy")],
            ),
            (
                "hex splits run",
                vec![snd_lit("a"), snd_hex(&[0x0d]), snd_lit("b")],
                vec![lit("a"), hex(&[0x0d]), lit("b")],
            ),
            // Raw bytes have no one-argument-per-key constraint, so adjacent payloads merge.
            (
                "adjacent hex merges",
                vec![snd_hex(&[0x0d]), snd_hex(&[0x0d])],
                vec![hex(&[0x0d, 0x0d])],
            ),
            (
                "interleaved hex and literals keep wire order",
                vec![
                    snd_hex(&paste_start[..]),
                    snd_lit("a"),
                    snd_hex(&[0x0d]),
                    snd_lit("b"),
                    snd_hex(&paste_end[..]),
                ],
                vec![
                    hex(&paste_start[..]),
                    lit("a"),
                    hex(&[0x0d]),
                    lit("b"),
                    hex(&paste_end[..]),
                ],
            ),
        ];
        for (name, input, expected) in cases {
            assert_eq!(coalesce(input), expected, "{name}");
        }
    }

    #[test]
    fn coalesce_resize_breaks_literal_run() {
        // A resize sandwiched between keystrokes must dispatch in order, so the agent
        // renders the trailing keys at the new geometry.
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
        // A paste must stay its own action: folding it into a literal run would put the
        // payload back on the `send-keys` path, where tmux never decides about the
        // bracketed-paste markers, and would drop the #1546 one-paste framing.
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
        // Without Ctrl, `q` is just a letter to send; translate no longer decides exit, but
        // the passthrough still needs a guard.
        assert_literal(translate(k(KeyCode::Char('q'))), "q");
        assert_literal(translate(k(KeyCode::Char('Q'))), "Q");
    }

    #[test]
    fn peel_trailing_semicolons_splits_trailing_run_only() {
        // tmux eats a trailing `;` from a `send-keys -l` payload, so the dispatcher peels
        // the trailing run and sends it as raw hex (#1942).
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
        let captures = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = captures.clone();
        let (worker, done) = LiveCaptureWorker::spawn_with_capture_for_test(
            std::sync::Arc::new(tokio::sync::Notify::new()),
            move || {
                observed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                (Some("geometry-ready".into()), None)
            },
        );
        worker.set_target("aoe_test_capture_no_geometry".into());
        wait_for_capture_cycle(&worker, &done, 0);
        assert_eq!(captures.load(std::sync::atomic::Ordering::Relaxed), 0);
        assert!(worker.take_latest().is_none());
        worker.set_capture_lines(40);
        assert_eq!(
            wait_for_latest(&worker, std::time::Duration::from_secs(5)).as_deref(),
            Some("geometry-ready")
        );
    }

    #[test]
    fn live_capture_worker_skips_empty_captures() {
        let captures = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = captures.clone();
        let (worker, done) = LiveCaptureWorker::spawn_with_capture_for_test(
            std::sync::Arc::new(tokio::sync::Notify::new()),
            move || {
                observed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                (Some(String::new()), None)
            },
        );
        worker.set_live(true);
        worker.set_capture_lines(40);
        worker.set_target("aoe_test_capture_empty".into());
        loop {
            wait_for_capture_cycle(&worker, &done, 40);
            if captures.load(std::sync::atomic::Ordering::Relaxed) > 0 {
                break;
            }
        }
        assert!(
            worker.take_latest().is_none(),
            "successful empty captures must not blank live preview"
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

    #[cfg(unix)]
    #[test]
    fn channel_arm_waits_for_settle_and_throttle() {
        let now = std::time::Instant::now();
        let settled = Some(now - std::time::Duration::from_millis(1));
        let resting = Some(now + std::time::Duration::from_millis(100));
        let cases = [
            ("unsettled selection", resting, false, None, false),
            ("settled, first attempt", settled, false, None, true),
            ("no settle window", None, false, None, true),
            ("already armed or in flight", settled, true, None, false),
            (
                "recent failed attempt",
                settled,
                false,
                Some(now - std::time::Duration::from_secs(1)),
                false,
            ),
            (
                "throttle elapsed",
                settled,
                false,
                Some(now - VT_REARM_INTERVAL),
                true,
            ),
        ];
        for (case, arm_after, armed, last_arm, expected) in cases {
            assert_eq!(
                channel_arm_due(arm_after, armed, last_arm, now),
                expected,
                "{case}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn pending_arm_delivers_only_the_current_generation() {
        let wake: CaptureWake =
            std::sync::Arc::new((std::sync::Mutex::new(0), std::sync::Condvar::new()));
        let (release, gate) = std::sync::mpsc::channel::<()>();
        let mut pending = Some(PendingArm::spawn(
            "pane".into(),
            7,
            wake.clone(),
            move |name, _| {
                gate.recv().ok()?;
                Some(std::sync::Arc::new(name.to_string()))
            },
        ));
        let mut stale = Vec::new();
        assert!(
            PendingArm::take(&mut pending, 7, &mut stale).is_none() && pending.is_some(),
            "a running arm stays pending"
        );
        release.send(()).unwrap();
        let mut observed = 0;
        wait_for_capture_wake(&wake, &mut observed, std::time::Duration::from_secs(5));
        assert_eq!(
            PendingArm::take(&mut pending, 7, &mut stale)
                .as_deref()
                .map(String::as_str),
            Some("pane"),
            "the current generation adopts its channel"
        );
        assert!(pending.is_none(), "a delivered arm is no longer pending");
        assert!(stale.is_empty());

        let (release, gate) = std::sync::mpsc::channel::<()>();
        let mut pending = Some(PendingArm::spawn(
            "pane".into(),
            7,
            wake.clone(),
            move |name, _| {
                gate.recv().ok()?;
                Some(std::sync::Arc::new(name.to_string()))
            },
        ));
        release.send(()).unwrap();
        wait_for_capture_wake(&wake, &mut observed, std::time::Duration::from_secs(5));
        assert!(
            PendingArm::take(&mut pending, 8, &mut stale).is_none(),
            "a retarget mid-arm never adopts the stale channel"
        );
        assert_eq!(
            stale.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            ["pane"],
            "the stale channel goes to the teardown sink"
        );
        assert!(
            pending.is_none(),
            "a stale arm no longer blocks the next one"
        );
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
        // The typed-echo case: no prior publish, or one long past, must never wait, or the
        // live-mode echo lag the event-driven wakeup kills comes back.
        assert_eq!(publish_floor_wait_ms(None), 0);
        assert_eq!(
            publish_floor_wait_ms(Some(LIVE_CAPTURE_INTERVAL_FAST_MS)),
            0
        );
        assert_eq!(publish_floor_wait_ms(Some(1_000)), 0);
    }

    #[test]
    fn publish_floor_paces_sustained_streaming_at_fast_cadence() {
        // Back-to-back changes must not publish faster than the fast interval: the 33ms
        // render ticker was calibrated against that pacing, and faster publishes tear on
        // terminals without synchronized updates.
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
        // A lone chunk (an echo, or the first after a quiet gap) reports
        // `streaming == false` and must sample with no added delay however recently it
        // landed, or the #2822 echo lag returns.
        assert_eq!(sample_debounce_wait_ms(false, 0, 0), 0);
        assert_eq!(sample_debounce_wait_ms(false, 0, 100), 0);
        assert_eq!(sample_debounce_wait_ms(false, 3, 0), 0);
    }

    #[test]
    fn sample_debounce_holds_active_stream_until_quiescent() {
        // While chunks arrive back-to-back and the stream has not gone quiet, a changed
        // frame is held so a multi-chunk repaint publishes once settled. The wait is the
        // remaining quiescence window.
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
        // Once the stream has been silent for the quiescence window, the settled frame
        // publishes immediately.
        assert_eq!(sample_debounce_wait_ms(true, SAMPLE_QUIESCENCE_MS, 10), 0);
        assert_eq!(
            sample_debounce_wait_ms(true, SAMPLE_QUIESCENCE_MS + 5, 10),
            0
        );
    }

    #[test]
    fn sample_debounce_latency_cap_bounds_sustained_streaming() {
        // A stream that never goes quiet must still render: a held frame publishes once it
        // waits out the latency cap, so heavy output paces at the cap rather than stalling.
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
        // Keystroke batches must dispatch without waiting on the size-owner check; putting
        // it back ahead of plain input re-creates the per-keystroke latency this classifier
        // avoids. Resizes keep verify-first so geometry never races another owner's grid.
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
        // Terminal / container panes set `forward_empty`, so a missing or cleared pane must
        // surface as an empty capture rather than being dropped like the agent kill switch.
        // Deterministic without tmux: a missing pane reads empty.
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
        // When a displayed agent/tool pane dies its capture fails rather than returning
        // empty content, and only `forward_empty` panes used to surface that. Outside
        // live-send a failed capture must publish an empty frame, so the preview shows "No
        // output available" instead of the dead pane's last bytes. Live mode keeps the
        // #1501 kill switch.
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
        // A budget change alone (deeper scroll over a quiet pane) must republish even when
        // the bytes are identical, or consumers waiting for a deeper capture stall forever.
        // Deterministic without tmux: forward-empty plus a missing pane is empty at every
        // budget.
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

    /// The worker never steals the size-owner lock back after entry: an external steal
    /// flips its sticky `lock_lost` flag, the thief keeps the lock, and a queued resize is
    /// dropped instead of stomping the new owner's grid. Fixes the tug-of-war where a
    /// background TUI's next keystroke reverted a phone takeover.
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

        // The next resize must verify, observe the loss, flag it and be dropped. The idle
        // heartbeat may flag it first; either path is the behavior under test.
        worker.resize(60, 20);
        wait_until("lock_lost flag", std::time::Duration::from_secs(5), || {
            worker.lock_lost()
        });
        assert_eq!(
            session.size_owner().map(|(id, _)| id),
            Some("live-test-thief".to_string()),
            "worker must not steal the lock back"
        );
        // Keys queued after the resize are dispatched after its batch, even
        // when ownership filtering removes the resize itself.
        worker.send(TmuxKey::Literal("RESIZE-BATCH-COMPLETE".into()));
        wait_until(
            "post-resize input reached the pane",
            std::time::Duration::from_secs(5),
            || {
                let output = crate::tmux::tmux_command()
                    .args(["capture-pane", "-p", "-t", guard.name()])
                    .output()
                    .expect("capture ordered input");
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout).contains("RESIZE-BATCH-COMPLETE")
            },
        );
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

    /// The entry steal can come up empty two ways: the pane has not appeared yet, or
    /// another surface won the confirm-read race. The retry path used to force-steal for
    /// both, silently stomping a live owner without flagging the loss. Spawning before the
    /// session exists reproduces `owned == false` deterministically.
    #[test]
    #[serial_test::serial]
    fn worker_defers_to_live_owner_when_entry_steal_found_no_session() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }
        let guard = crate::tmux::test_helpers::TmuxTestSession::new("aoe_test_livelock_late");
        // A failed resize acknowledges that the worker observed the absent session.
        let worker = LiveSendWorker::spawn(guard.name().to_string(), None);
        worker.resize(60, 20);
        wait_until(
            "resize against absent session",
            std::time::Duration::from_secs(5),
            || worker.take_resize_failed(),
        );

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

        // The new session has a live owner before the next resize.
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
        // Keys queued after the resize are dispatched after its batch, even
        // when ownership filtering removes the resize itself.
        worker.send(TmuxKey::Literal("RESIZE-BATCH-COMPLETE".into()));
        wait_until(
            "post-resize input reached the pane",
            std::time::Duration::from_secs(5),
            || {
                let output = crate::tmux::tmux_command()
                    .args(["capture-pane", "-p", "-t", guard.name()])
                    .output()
                    .expect("capture ordered input");
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout).contains("RESIZE-BATCH-COMPLETE")
            },
        );
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
        worker.resize(60, 20);
        wait_until(
            "resize against absent session",
            std::time::Duration::from_secs(5),
            || worker.take_resize_failed(),
        );

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
