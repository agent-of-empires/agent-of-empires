//! Main TUI application

use anyhow::{Context, Result};
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
    MouseButton, MouseEventKind, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use futures_util::StreamExt;
use ratatui::prelude::*;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use super::home::{HomeView, TerminalMode};
use super::styles::Theme;
use crate::containers::image_update::ImageUpdate;
use crate::session::{get_update_settings, update_app_state, Config};
use crate::tmux::AvailableTools;
use crate::update::{check_for_update, UpdateInfo};

/// Minimum elapsed time between considering periodic update re-checks.
/// The main loop runs at ~20Hz; gating on this gap keeps the per-iteration
/// `get_update_settings()` config read off the hot path while still
/// re-evaluating well under the daily re-check interval.
const UPDATE_CHECK_THROTTLE_GAP: Duration = Duration::from_secs(60);

/// How often a long-running TUI re-checks for updates, matching the
/// `check_for_update` cache TTL (`update::UPDATE_CHECK_INTERVAL_HOURS`).
const PERIODIC_RECHECK_INTERVAL: Duration =
    Duration::from_secs(crate::update::UPDATE_CHECK_INTERVAL_HOURS * 3600);

/// Inter-key timeout for the paste-burst detector. After any printable Char
/// or Enter, the event loop polls for the next event with this timeout; if
/// another burst-candidate arrives before the deadline, it joins the burst.
/// Mosh strips bracketed-paste markers, so dictation from iOS clients lands
/// as a tightly-packed stream of individual key events; 5ms is comfortably
/// wider than a Mosh paste's inter-key gap and well under any human typing
/// rhythm, so it discriminates between paste and typing without making
/// single-key shortcuts feel laggy.
const PASTE_BURST_INTER_KEY_MS: u64 = 5;

/// Minimum length (in burst-candidate events) for an accumulated burst to be
/// routed through `handle_paste`. Shorter accumulations are replayed as
/// individual key events so genuine typing isn't mistaken for a paste.
const PASTE_BURST_MIN_LEN: usize = 3;

/// Process-local session-create trend counter for the TUI surface, mirroring the
/// serve daemon's `telemetry_session_creates` (#1897). A long-lived TUI creates
/// sessions over its lifetime; this monotonic accumulator carries that count
/// into the opt-in `usage_snapshot.session_creates_since_last_snapshot` field.
/// It is incremented on each create in [`record_session_create`], read
/// without reset when a snapshot is built, and decremented by exactly the
/// reported value only after a confirmed send so a create that lands during an
/// in-flight send rolls into the next snapshot rather than being double-counted
/// or dropped. A no-op for opted-out installs (no snapshot is ever sent).
static TUI_SESSION_CREATES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Point-in-time read of the create counter, so a snapshot can later be cleared
/// by exactly the value it reported.
fn reported_session_creates() -> u32 {
    TUI_SESSION_CREATES.load(std::sync::atomic::Ordering::Relaxed)
}

/// Decrement the create counter by exactly `reported` after a confirmed send,
/// mirroring serve's `decrement_reported_count`. Subtracting the reported amount
/// rather than zeroing preserves any create that landed between the snapshot
/// build and the confirmed send. A no-op when nothing was reported or the send
/// was not confirmed (`Deduped`/`Failed` retain the count for the next snapshot).
/// The subtraction saturates rather than underflow-wrapping the `AtomicU32`.
fn clear_reported_session_creates(reported: u32, outcome: crate::telemetry::SendOutcome) {
    if reported == 0 || outcome != crate::telemetry::SendOutcome::Sent {
        return;
    }
    use std::sync::atomic::Ordering;
    // `try_update` needs Rust 1.99; this keeps the 1.85 MSRV and the Nix toolchain building.
    #[allow(deprecated)]
    let _ = TUI_SESSION_CREATES.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_sub(reported))
    });
}

/// Count one TUI session create for the opt-in telemetry trend counter.
/// Bounded accumulator, read-and-decremented by the snapshot paths; a no-op
/// for opted-out installs. Called when a daemon-confirmed creation lands.
pub(super) fn record_session_create() {
    TUI_SESSION_CREATES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

struct UpdateStatus {
    text: String,
    expires_at: Option<std::time::Instant>,
}

impl UpdateStatus {
    fn persistent(text: String) -> Self {
        Self {
            text,
            expires_at: None,
        }
    }

    fn transient(text: String) -> Self {
        Self {
            text,
            expires_at: Some(std::time::Instant::now() + std::time::Duration::from_secs(10)),
        }
    }

    fn is_expired(&self) -> bool {
        match self.expires_at {
            Some(deadline) => std::time::Instant::now() >= deadline,
            None => false,
        }
    }
}

/// The terminal backend the TUI runs on: crossterm plus OSC 8 re-emission for
/// the cells the renderer marked as hyperlinks.
pub type TuiBackend = crate::tui::hyperlink::HyperlinkBackend<std::io::Stdout>;

pub struct App {
    home: HomeView,
    should_quit: bool,
    theme: Theme,
    /// Identity of the currently applied `theme` (global theme name +
    /// palette-downsample mode). `set_theme` compares against this so a
    /// re-apply with the same identity is a no-op. The config-file watcher
    /// re-dispatches the theme on EVERY `config.toml` save (it can't tell
    /// what changed), and a needless `set_theme` there sets `needs_redraw`,
    /// forcing a full-screen `clear_terminal` that flickers. Guarding here
    /// keeps any config save (collapse persistence, list resize, `i`,
    /// settings) from clearing the screen when the theme is unchanged.
    theme_name: String,
    theme_palette_mode: bool,
    needs_redraw: bool,
    update_info: Option<UpdateInfo>,
    update_rx: Option<tokio::sync::oneshot::Receiver<anyhow::Result<UpdateInfo>>>,
    update_status: Option<UpdateStatus>,
    update_status_rx: Option<tokio::sync::oneshot::Receiver<anyhow::Result<()>>>,
    /// Latest version the user dismissed via Ctrl+x. Persisted to
    /// `app_state.dismissed_update_version` so the snooze survives
    /// `aoe` restarts (per #1140). The banner stays hidden while the
    /// fetched latest_version equals this value, and returns
    /// automatically when a newer release ships.
    dismissed_update_version: Option<String>,
    /// A newer sandbox image detected in its registry, surfaced as the
    /// lowest-priority bottom banner (below app-update and transient status).
    /// `None` until the background check finds a drift the user hasn't snoozed.
    image_update: Option<ImageUpdate>,
    image_update_rx: Option<tokio::sync::oneshot::Receiver<anyhow::Result<Option<ImageUpdate>>>>,
    /// In-flight `docker pull` of the sandbox image, kicked off when the user
    /// accepts the banner's confirm. Result promotes into a transient toast.
    image_pull_rx: Option<tokio::sync::oneshot::Receiver<anyhow::Result<()>>>,
    /// Registry digest the user dismissed via Ctrl+x on the image banner.
    /// Persisted to `app_state.dismissed_image_digest`; the banner stays hidden
    /// while the registry still resolves to this digest.
    dismissed_image_digest: Option<String>,
    /// Held in an Option so `with_raw_mode_disabled` can drop it before
    /// spawning child processes. Crossterm's EventStream runs a background
    /// reader thread on stdin; if it's alive when tmux attach-session starts,
    /// the two compete for stdin and tmux fails to initialize its client.
    event_stream: Option<EventStream>,
    /// Tracks whether we currently have xterm mouse-tracking enabled. The TUI
    /// turns it off while a copy-friendly surface is open (`HomeView::
    /// wants_text_selection`) so users can drag-select natively, then turns
    /// it back on when the surface dismisses. Default true to match the
    /// startup `EnableMouseCapture` in `tui::run`.
    mouse_captured: bool,
    /// Whether the resolved config permits xterm mouse tracking (the
    /// `session.mouse_capture` field plus the `AOE_MOUSE_CAPTURE` backstop).
    /// This is permission, not live state: `mouse_captured` tracks whether
    /// tracking is actually engaged right now. Refreshed from disk on the
    /// periodic reload so toggling Settings > Interaction > Mouse Capture takes
    /// effect without a restart. When false, `sync_mouse_capture` keeps xterm
    /// tracking off entirely.
    mouse_capture_allowed: bool,
    /// Last OSC 0 host-tab title written. Dedups unchanged selections and
    /// is invalidated after `tmux attach` so the dashboard title is restored.
    host_title: super::host_title::HostTitleTracker,
    /// True when running under Mosh (`MOSH_CONNECTION` set). Mosh mangles
    /// xterm mouse-tracking escapes, so `tui::run` skips the startup
    /// `EnableMouseCapture` and `sync_mouse_capture` must not re-enable
    /// tracking mid-session either.
    mosh_active: bool,
    /// Set by `Action::OpenStructuredView` so the async main loop can pick it
    /// up and enter the acp view (which needs `event_stream` access
    /// the sync `execute_action` can't lend out).
    pending_structured_view_open: Option<String>,
    /// Set by `Action::SwitchSessionView` so the async main loop can run
    /// the daemon switch POST (awaited; the sync handler can't).
    pending_view_switch: Option<String>,
    /// Set by `Action::StartDaemonThenOpenStructured` (the Yes on the
    /// "start a local daemon?" confirm) so the async loop can spawn the
    /// daemon, wait for health, and then open the structured view.
    pending_daemon_start_open: Option<String>,
    /// Set by `Action::SmartRenameNow` so the async loop can run the daemon
    /// `/smart-rename` POST for a structured session (#3039).
    pending_smart_rename: Option<String>,
    /// Debounce for structured preview-on-select: the session the cursor
    /// settled on and when, so rapid list navigation doesn't connect a
    /// WebSocket per keystroke. The mounted view itself lives on
    /// `HomeView::structured_preview` (it is preview content); this App
    /// side only drives the async mount/unmount.
    preview_mount_pending: Option<(String, std::time::Instant)>,
    /// Version of the install currently being attempted (auto or manual).
    /// Set when the install task is spawned; transferred to
    /// `last_installed_version_in_session` on confirmed success in
    /// `poll_update_status`. Cleared on failure so the user can retry.
    pending_install_version: Option<String>,
    /// Version we successfully installed this session. The running binary's
    /// compile-time `CARGO_PKG_VERSION` stays at the old value until
    /// restart, so without this guard every periodic re-check (#1471) would
    /// surface the same release again: as an auto-install loop in auto
    /// mode, and as a re-appearing banner in notify mode. A genuinely newer
    /// release clears the guard automatically because the version string
    /// differs. Single-process scope; on restart the new binary's
    /// `CARGO_PKG_VERSION` makes the underlying check return "no update".
    last_installed_version_in_session: Option<String>,
}

/// Check if the app version changed and return the previous version if changelog should be shown.
/// This is called before App::new to allow async cache refresh.
pub fn check_version_change() -> Result<Option<String>> {
    let config = Config::load_or_warn();
    let current_version = env!("CARGO_PKG_VERSION");

    if config.app_state.has_seen_welcome
        && config.app_state.last_seen_version.as_deref() != Some(current_version)
    {
        Ok(config.app_state.last_seen_version)
    } else {
        Ok(None)
    }
}

/// Whether applying `next` `(theme name, palette-downsample mode)` would change
/// the active theme `current`. Pulled out of `App::set_theme` so the
/// idempotency guard (which keeps a config-file-watcher theme re-dispatch from
/// forcing a flickering full-screen clear on every `config.toml` save) is
/// unit-testable without constructing a full `App`.
fn theme_apply_needed(current: (&str, bool), next: (&str, bool)) -> bool {
    current != next
}

/// RAII guard that ignores `SIGINT` and `SIGQUIT` for as long as it's
/// alive, restoring whatever disposition was in effect beforehand on drop.
///
/// `with_raw_mode_disabled` calls `disable_raw_mode()` before handing the
/// terminal to a child process (tmux attach, an editor shell-out). With raw
/// mode off, the kernel goes back to delivering keyboard-generated signals
/// to aoe's own foreground process group. If the child pane is dead or
/// hung and the user hits Ctrl+C to escape, that SIGINT lands on aoe
/// itself, not just the child, and aoe has no handler for it: it dies
/// immediately with zero cleanup, taking down every tmux session/pane it
/// was managing. Holding this guard for the duration of the closure closes
/// that window.
#[cfg(unix)]
struct IgnoreSignalsGuard {
    prev_sigint: Option<nix::sys::signal::SigAction>,
    prev_sigquit: Option<nix::sys::signal::SigAction>,
}

#[cfg(unix)]
impl IgnoreSignalsGuard {
    fn new() -> Self {
        use nix::sys::signal::{sigaction, SaFlags, SigAction, SigHandler, SigSet, Signal};

        let ignore = SigAction::new(SigHandler::SigIgn, SaFlags::empty(), SigSet::empty());

        // SAFETY: SIG_IGN is async-signal-safe per POSIX, which is the only
        // requirement for sigaction calls made outside a signal handler.
        let prev_sigint = unsafe { sigaction(Signal::SIGINT, &ignore) }
            .inspect_err(|e| tracing::warn!(target: "tui.input", "Failed to ignore SIGINT: {}", e))
            .ok();
        // SAFETY: see above.
        let prev_sigquit = unsafe { sigaction(Signal::SIGQUIT, &ignore) }
            .inspect_err(|e| tracing::warn!(target: "tui.input", "Failed to ignore SIGQUIT: {}", e))
            .ok();

        Self {
            prev_sigint,
            prev_sigquit,
        }
    }
}

#[cfg(unix)]
impl Drop for IgnoreSignalsGuard {
    fn drop(&mut self) {
        use nix::sys::signal::{sigaction, Signal};

        if let Some(prev) = self.prev_sigint.take() {
            // SAFETY: restoring a saved disposition only mutates process-wide signal state.
            let _ = unsafe { sigaction(Signal::SIGINT, &prev) };
        }
        if let Some(prev) = self.prev_sigquit.take() {
            // SAFETY: see above.
            let _ = unsafe { sigaction(Signal::SIGQUIT, &prev) };
        }
    }
}

/// Whether `draw` should skip its explicit pre-render `cursor::Hide`.
///
/// The pre-draw Hide exists only to keep an IME candidate window from being
/// dragged by the backend's transient cursor moves during the diff paint. In
/// live-send with no overlay open, the only cursor is the remote preview-pane
/// caret (no local IME), and the early Hide (flushed before the ~2-3ms widget
/// build, while ratatui's trailing Show is flushed after it) is what straddles
/// the vsync boundary and reads as ~30fps flicker on terminals without
/// synchronized-update (Terminal.app). Skipping it there removes the blink;
/// every other state keeps the Hide.
fn skip_predraw_cursor_hide(live_send_active: bool, has_overlay: bool) -> bool {
    live_send_active && !has_overlay
}

impl App {
    fn set_status(&mut self, text: impl Into<String>) {
        self.update_status = Some(UpdateStatus::transient(text.into()));
    }
    /// Is this key event a candidate for paste-burst accumulation?
    /// Printable ASCII Char or Enter, with no modifiers (or shift only).
    /// Burst detection ignores Ctrl/Alt-modified chords because those
    /// are genuine intentional shortcuts and never come from a paste-burst.
    /// Enter is included so embedded CR/LF inside a Mosh-stripped paste
    /// (voice/dictation often inserts sentence-break newlines) gets
    /// captured into the burst as \n instead of breaking it in two and
    /// firing Submit/select on the deferred Enter.
    fn is_burst_candidate(key: &KeyEvent) -> bool {
        let mods = key.modifiers;
        let mods_ok = mods.is_empty() || mods == KeyModifiers::SHIFT;
        if !mods_ok {
            return false;
        }
        match key.code {
            KeyCode::Char(c) => c == ' ' || c.is_ascii_graphic(),
            KeyCode::Enter => true,
            _ => false,
        }
    }

    /// Translate a burst-candidate key event back to its text byte for the
    /// accumulated burst string. Char yields the char; Enter yields '\n'.
    fn burst_char_for(key: &KeyEvent) -> Option<char> {
        match key.code {
            KeyCode::Char(c) => Some(c),
            KeyCode::Enter => Some('\n'),
            _ => None,
        }
    }

    /// Holding a key produces a stream of Press events on terminals that do not
    /// report key-event types, or an initial Press followed by Repeat events on
    /// terminals that do. That stream has the same timing as Mosh's paste
    /// fallback, but it is navigation, not pasted text. Keep it on the normal
    /// input path so held `j`/`k` continue scrolling the session list instead of
    /// opening the message composer.
    fn is_auto_repeat_burst(keys: &[KeyEvent]) -> bool {
        let Some(first) = keys.first() else {
            return false;
        };
        keys.iter()
            .skip(1)
            .all(|key| key.code == first.code && key.modifiers == first.modifiers)
    }

    /// Peel a trailing Enter off a paste burst so plain-Enter Submit
    /// semantics survive when the user types or dictates fast enough to
    /// pump everything through the burst path.
    ///
    /// Without this peel, an "hi<Enter>" with sub-5ms key gaps
    /// (fast typing, clipboard paste with trailing newline, VoiceInk
    /// dictation that punctuates with a return) lands as a single
    /// burst `[h, i, Enter]` whose string is `"hi\n"`. The current
    /// code forwards that whole string through `handle_paste`, which
    /// inserts `\n` as a literal newline in the textarea, so the
    /// `Enter` never reaches the dialog's Submit branch and the
    /// message never sends.
    ///
    /// The fix preserves embedded `\n` (mid-burst sentence breaks from
    /// Mosh-stripped voice paste; the original reason Enter was added
    /// to `is_burst_candidate`) and only peels the trailing Enter,
    /// which is intent-to-submit, not data.
    ///
    /// Returns `(paste_text, trailing_enter)`:
    ///   * `paste_text`: the string to forward to `handle_paste` with
    ///     any trailing `\n` removed.
    ///   * `trailing_enter`: `Some(KeyEvent)` to replay via
    ///     `handle_key` after `handle_paste` runs, so the dialog's
    ///     plain-Enter Submit branch fires; `None` if the burst did
    ///     not end on Enter.
    fn split_trailing_enter(
        burst_str: &str,
        burst_keys: &[KeyEvent],
    ) -> (String, Option<KeyEvent>) {
        match burst_keys.last() {
            Some(last) if last.code == KeyCode::Enter => {
                let trimmed = burst_str
                    .strip_suffix('\n')
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| burst_str.to_string());
                (trimmed, Some(*last))
            }
            _ => (burst_str.to_string(), None),
        }
    }

    /// The per-frame hyperlink map the renderer fills, for the terminal backend
    /// to re-emit as OSC 8. Handed out after construction so `run` can build the
    /// backend around the same map the `HomeView` writes into.
    pub fn hyperlink_cells(&self) -> crate::tui::hyperlink::SharedHyperlinks {
        self.home.hyperlink_cells.clone()
    }

    pub fn new(
        profile: &str,
        available_tools: AvailableTools,
        suppress_first_run_dialogs: bool,
        mosh_active: bool,
        file_watch: std::sync::Arc<crate::file_watch::FileWatchService>,
    ) -> Result<Self> {
        let no_agents = !available_tools.any_available();
        let active_profile = if profile.is_empty() {
            None // all-profiles mode
        } else {
            Some(profile.to_string())
        };
        let mut home = HomeView::new(active_profile, available_tools, file_watch)?;

        // Check if we need to show welcome or changelog dialogs
        let config = Config::load_or_warn();
        // Theme is a global preference, never profile-merged.
        let theme_name = config.effective_theme_name();
        let palette_mode = config.theme_palette_mode();
        let theme = crate::tui::styles::load_theme_with_mode(&theme_name, palette_mode);
        let current_version = env!("CARGO_PKG_VERSION").to_string();

        if no_agents {
            // Show the no-agents onboarding dialog (takes priority over welcome/changelog)
            home.show_no_agents();
        } else if suppress_first_run_dialogs {
            // The caller shows a startup warning first.
        } else if !config.app_state.has_seen_welcome {
            home.show_intro(&theme_name);
            if let Err(e) = update_app_state(|state| {
                state.has_seen_welcome = true;
                state.last_seen_version = Some(current_version.clone());
            }) {
                tracing::warn!(
                    target: "tui.startup",
                    error = %e,
                    "failed to persist has_seen_welcome/last_seen_version"
                );
            }
        } else if config.app_state.last_seen_version.as_deref() != Some(&current_version) {
            // Cache should already be refreshed by tui::run() before App::new
            home.show_changelog(config.app_state.last_seen_version.clone());
            if let Err(e) = update_app_state(|state| {
                state.last_seen_version = Some(current_version.clone());
            }) {
                tracing::warn!(
                    target: "tui.startup",
                    error = %e,
                    "failed to persist last_seen_version"
                );
            }
        } else if !config.app_state.has_responded_to_telemetry {
            // One-time opt-in for users who finished onboarding before telemetry existed.
            home.show_telemetry_consent();
        }

        let dismissed_update_version = config.app_state.dismissed_update_version.clone();
        let dismissed_image_digest = config.app_state.dismissed_image_digest.clone();

        Ok(Self {
            home,
            should_quit: false,
            theme,
            theme_name,
            theme_palette_mode: palette_mode,
            needs_redraw: true,
            update_info: None,
            update_rx: None,
            update_status: None,
            update_status_rx: None,
            dismissed_update_version,
            image_update: None,
            image_update_rx: None,
            image_pull_rx: None,
            dismissed_image_digest,
            // Crossterm's stream needs a live event reader, which tests lack.
            event_stream: (!cfg!(test)).then(EventStream::new),
            // Initial state matches whatever `tui::run` did at startup: capture
            // is requested by default, but Mosh suppresses the actual escape, so
            // `mouse_captured` (live state) also factors in `mosh_active`.
            // `mouse_capture_allowed` is permission only and ignores Mosh.
            mouse_captured: crate::tui::mouse_capture_requested(&config.session) && !mosh_active,
            mouse_capture_allowed: crate::tui::mouse_capture_requested(&config.session),
            host_title: super::host_title::HostTitleTracker::default(),
            mosh_active,
            pending_structured_view_open: None,
            pending_daemon_start_open: None,
            preview_mount_pending: None,
            pending_view_switch: None,
            pending_smart_rename: None,
            pending_install_version: None,
            last_installed_version_in_session: None,
        })
    }

    fn sync_mouse_capture(&mut self, terminal: &mut Terminal<TuiBackend>) -> Result<()> {
        let desired =
            self.mouse_capture_allowed && !self.mosh_active && !self.home.wants_text_selection();
        if desired == self.mouse_captured {
            return Ok(());
        }
        if desired {
            crossterm::execute!(terminal.backend_mut(), EnableMouseCapture)?;
        } else {
            crossterm::execute!(terminal.backend_mut(), DisableMouseCapture)?;
        }
        self.mouse_captured = desired;
        Ok(())
    }

    /// Write OSC 0 when the dashboard selection (or its title) changes.
    ///
    /// Emitted after the frame so it does not interleave with OSC 8 runs
    /// inside `HyperlinkBackend::draw`. No-ops when the setting is off
    /// and we have never written, so opt-out users keep the terminal's
    /// own naming.
    fn sync_host_title(&mut self, terminal: &mut Terminal<TuiBackend>) -> Result<()> {
        let Some(title) = self
            .host_title
            .sync(self.home.host_tab_title, self.home.selected_session_title())
        else {
            return Ok(());
        };
        crossterm::execute!(terminal.backend_mut(), crossterm::terminal::SetTitle(title))?;
        super::host_title::note_emitted();
        Ok(())
    }

    /// Draw a frame without exposing ratatui's intermediate cursor moves.
    ///
    /// The backend moves the real terminal cursor while flushing changed
    /// cells. If an IME is composing text, those transient moves can pull the
    /// candidate window toward refreshed UI such as the status list before the
    /// frame's final cursor position is restored. Synchronized update batches
    /// the frame, and hiding the cursor before the batch keeps the only visible
    /// cursor transition at ratatui's final `Frame::set_cursor_position`.
    ///
    /// `skip_predraw_cursor_hide` skips that pre-draw Hide specifically in
    /// live-send with no overlay open: that is the one state that visibly
    /// flickers on terminals without synchronized-update support (Terminal.app),
    /// because the only cursor set in that state is the remote live-preview
    /// pane caret, not a local IME candidate window, so there's nothing for
    /// the early Hide to protect.
    fn draw(&mut self, terminal: &mut Terminal<TuiBackend>) -> Result<()> {
        // A visible caret (active embedded view, or live-send without an
        // overlay) strobes if hidden before every redraw.
        let embedded_active = self
            .home
            .structured_preview
            .as_ref()
            .is_some_and(|v| v.is_active());
        let skip_hide = embedded_active
            || skip_predraw_cursor_hide(
                self.home.live_send.is_some(),
                self.home.has_non_live_send_overlay(),
            );
        // QUEUE, never execute: `execute!` flushes, and flushing the batch
        // opener on its own puts the ~10ms widget build INSIDE the
        // synchronized-update bracket, so the terminal holds its display
        // frozen for a third of every frame instead of batching the result of
        // one. Queued, these ride out in `terminal.draw`'s own flush together
        // with the cells and the trailing Show, which is the whole point of
        // the bracket: one write, one atomic frame.
        crossterm::queue!(
            terminal.backend_mut(),
            crossterm::terminal::BeginSynchronizedUpdate
        )?;
        let draw_result = (|| -> Result<()> {
            if !skip_hide {
                crossterm::queue!(terminal.backend_mut(), crossterm::cursor::Hide)?;
            }
            terminal.draw(|f| self.render(f))?;
            Ok(())
        })();
        let end_result = crossterm::execute!(
            terminal.backend_mut(),
            crossterm::terminal::EndSynchronizedUpdate
        );
        draw_result?;
        end_result?;
        self.sync_host_title(terminal)?;
        Ok(())
    }

    /// Temporarily leave TUI mode, run a closure, and restore TUI mode.
    /// Drops the EventStream before the closure so child processes (tmux,
    /// editors) have exclusive access to stdin, then creates a fresh one.
    fn with_raw_mode_disabled<F, R>(
        &mut self,
        terminal: &mut Terminal<TuiBackend>,
        f: F,
    ) -> Result<R>
    where
        F: FnOnce() -> R,
    {
        crossterm::terminal::disable_raw_mode()?;
        // Popped and repushed around the child so tmux sees a clean terminal.
        #[cfg(unix)]
        let _ = crossterm::execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags);
        crossterm::execute!(
            terminal.backend_mut(),
            crossterm::terminal::LeaveAlternateScreen,
            DisableBracketedPaste,
        )?;
        if self.mouse_captured {
            crossterm::execute!(terminal.backend_mut(), DisableMouseCapture)?;
        }
        crossterm::execute!(terminal.backend_mut(), crossterm::cursor::Show)?;
        self.mouse_captured = false;
        std::io::Write::flush(terminal.backend_mut())?;

        // Drop the event stream so its background reader releases stdin.
        // Without this, tmux attach-session fails because crossterm's
        // reader thread competes for stdin reads.
        self.event_stream.take();

        // Raw mode is off from here until it's re-enabled below, so the
        // kernel is delivering Ctrl+C/Ctrl+\ to aoe's own foreground process
        // group again. Ignore them for the handoff so a Ctrl+C meant for a
        // hung/dead child pane can't kill aoe out from under every session
        // it's managing (the tokio SIGINT arm still catches anything that
        // slips past this window before raw mode is re-enabled).
        #[cfg(unix)]
        let _signals_guard = IgnoreSignalsGuard::new();

        let result = f();

        #[cfg(unix)]
        drop(_signals_guard);

        crossterm::terminal::enable_raw_mode()?;
        crossterm::execute!(
            terminal.backend_mut(),
            crossterm::terminal::EnterAlternateScreen,
            EnableBracketedPaste,
            crossterm::cursor::Hide
        )?;
        // Repush the kitty enhancement stack symmetric with the pop above
        // (#2362). Best-effort, mirrors `TerminalGuard::enter`.
        #[cfg(unix)]
        let _ = crossterm::execute!(
            terminal.backend_mut(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES),
        );
        // Defer mouse-capture restore to sync_mouse_capture so we don't
        // briefly enable it only to disable again when the user returned
        // to the serve view. sync_mouse_capture itself respects the Mouse
        // Capture setting and the AOE_MOUSE_CAPTURE opt-out.
        self.sync_mouse_capture(terminal)?;
        // Attach may have overwritten the host tab via the pane's OSC 0.
        self.host_title.invalidate();
        self.sync_host_title(terminal)?;
        std::io::Write::flush(terminal.backend_mut())?;

        // Recreated only after raw mode is back, so it isn't born on a cooked tty.
        self.event_stream = Some(EventStream::new());
        crate::tui::clear_terminal(terminal)?;
        #[cfg(feature = "e2e-tests")]
        if let Some(path) = std::env::var_os("AOE_E2E_INPUT_BARRIER") {
            let path = std::path::PathBuf::from(path).with_extension("resumed");
            let previous = match std::fs::read_to_string(&path) {
                Ok(value) => value.parse::<u64>()?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
                Err(error) => return Err(error.into()),
            };
            std::fs::write(path, (previous + 1).to_string())?;
        }

        Ok(result)
    }

    pub fn show_startup_warning(&mut self, message: &str) {
        // Warnings preempt onboarding dialogs.
        self.home.intro_dialog = None;
        self.home.changelog_dialog = None;
        self.home.telemetry_consent_dialog = None;
        tracing::info!(target: "tui.dialog", dialog = "warning", "opening warning dialog");
        self.home.info_dialog = Some(crate::tui::dialogs::InfoDialog::sized_to_fit(
            "Warning", message,
        ));
    }

    pub fn set_theme(&mut self, name: &str) {
        // Theme and color mode are global; reapplying an unchanged theme would
        // force a flickering full clear on every config save.
        let palette_mode = crate::session::config::resolve_theme_palette_mode();
        // No-op when the theme is already applied. The config watcher
        // re-dispatches the theme on every `config.toml` save, so without
        // this a list-resize / `i` / collapse-persistence / settings save
        // would force a full-screen `clear_terminal` and flicker even though
        // nothing visual changed.
        if !theme_apply_needed(
            (&self.theme_name, self.theme_palette_mode),
            (name, palette_mode),
        ) {
            return;
        }
        self.theme = crate::tui::styles::load_theme_with_mode(name, palette_mode);
        self.theme_name = name.to_string();
        self.theme_palette_mode = palette_mode;
        self.needs_redraw = true;
    }

    pub async fn run(&mut self, terminal: &mut Terminal<TuiBackend>) -> Result<()> {
        self.home.connect_runtime();
        // Keep the display snapshots (sessions, pane metadata) fresh off the
        // paint thread: every _for_display helper and the passive preview
        // resize executor answers from these snapshots and never forks in render.
        // The poller's first cycle runs immediately. Do not warm the cache
        // here: tmux may consume the full command deadline, and startup must
        // paint its conservative empty snapshot before any such wait.
        crate::tmux::spawn_snapshot_poller();

        // Initial render
        crate::tui::clear_terminal(terminal)?;
        // This clear satisfies any pending redraw; honoring it on the first tick
        // would blank the first frame until the next paint.
        self.needs_redraw = false;
        // Sync mouse capture before the first paint so any onboarding
        // surface that wants native drag-to-select (intro Welcome page,
        // changelog, info dialog) gets capture turned off on frame 1.
        // Otherwise the user would have to press a key first.
        self.sync_mouse_capture(terminal)?;
        self.draw(terminal)?;
        #[cfg(feature = "e2e-tests")]
        e2e_render_ack(true)?;

        // `None` when checks are off, so enabling them later checks immediately.
        let settings = get_update_settings();
        let mut last_update_check: Option<std::time::Instant> =
            if settings.update_check_mode.is_enabled() {
                self.spawn_update_check();
                Some(std::time::Instant::now())
            } else {
                None
            };

        // Only for users who run sandboxed sessions.
        if settings.update_check_mode.is_enabled() && self.sandbox_in_use() {
            self.spawn_image_update_check();
        }

        // Exit cleanly when the terminal is force-quit, preventing PTY slot leaks.
        #[cfg(unix)]
        let (mut sighup, mut sigterm, mut sigint) = {
            use tokio::signal::unix::{signal, SignalKind};
            let hup = signal(SignalKind::hangup());
            let term = signal(SignalKind::terminate());
            let int = signal(SignalKind::interrupt());
            if let Err(ref e) = hup {
                tracing::warn!(target: "tui.input", "Failed to register SIGHUP handler: {}", e);
            }
            if let Err(ref e) = term {
                tracing::warn!(target: "tui.input", "Failed to register SIGTERM handler: {}", e);
            }
            if let Err(ref e) = int {
                tracing::warn!(target: "tui.input", "Failed to register SIGINT handler: {}", e);
            }
            (hup.ok(), term.ok(), int.ok())
        };

        // 33ms (~30fps): 16ms tore on terminals without synchronized update.
        let mut refresh_interval = tokio::time::interval(Duration::from_millis(33));
        refresh_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // One extra refresh ~15ms after a live-send key catches the agent's echo.
        let mut last_live_key_at: Option<std::time::Instant> = None;
        const POST_KEY_WAKE_DELAY: Duration = Duration::from_millis(15);
        // Skip ticker refreshes right after another refresh to avoid tearing.
        let mut last_refresh_at: Option<std::time::Instant> = None;
        const REFRESH_COOLDOWN: Duration = Duration::from_millis(15);
        let mut last_metrics_sample = std::time::Instant::now();
        let mut last_disk_refresh = std::time::Instant::now();
        let mut full_heartbeat_deferred = false;
        let mut last_spinner_redraw = std::time::Instant::now();
        let mut last_heartbeat = std::time::Instant::now();
        let mut last_presence_refresh = std::time::Instant::now();
        // Throttle for how often the periodic block re-reads settings;
        // without this, the inner guards would re-fire on every loop
        // iteration once any time has passed, hitting the config file at
        // the 20Hz loop rate.
        let mut last_update_eval = std::time::Instant::now();
        const DISK_REFRESH_INTERVAL: Duration = Duration::from_secs(5);
        // Diagnostics-strip sampling. 1s keeps the sparkline responsive to a
        // fast memory climb; request_metrics_refresh is a no-op unless the strip
        // is visible, so this costs nothing when the pane is hidden.
        const METRICS_SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
        // Fastest spinner (breathe) changes every 180ms.
        const SPINNER_REDRAW_INTERVAL: Duration = Duration::from_millis(120);
        const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
        // How often to recount live TUIs for the footer indicator. Cheap dir
        // listing (a handful of entries), so a tight-ish cadence keeps the
        // "another instance appeared/left" signal responsive without disk I/O
        // on the hot render path.
        const PRESENCE_REFRESH_INTERVAL: Duration = Duration::from_secs(3);
        // A presence file counts as live while its mtime is within this window.
        // Larger than the liveness heartbeat so a couple of missed beats (busy loop,
        // brief stall) don't drop an instance; matches the push consumer.
        const PRESENCE_FRESH_WINDOW: Duration = Duration::from_secs(30);

        // Register this TUI as live for the footer, and as recently interacted
        // with for short-lived push suppression.
        crate::session::write_tui_heartbeat();
        crate::session::write_tui_activity();
        self.home.active_tui_count = crate::session::count_active_tuis(PRESENCE_FRESH_WINDOW);

        // Telemetry is opt-in; sends are detached and swallow errors.
        let telemetry_snapshot_interval = crate::telemetry::snapshot_interval();
        crate::telemetry::spawn_process_start(crate::telemetry::Surface::Tui);
        self.emit_telemetry_snapshot();
        let mut last_telemetry_snapshot = std::time::Instant::now();

        loop {
            // Force full redraw if needed (e.g., after returning from tmux).
            // with_raw_mode_disabled drops and recreates the EventStream, so
            // there are no stale events to drain.
            if self.needs_redraw {
                crate::tui::clear_terminal(terminal)?;
                self.needs_redraw = false;
            }

            // Compute the post-key wake deadline once per iteration so
            // the select! arm doesn't have to dance with the Option.
            // `None` here becomes `pending` inside the arm.
            let post_key_deadline = last_live_key_at.map(|t| t + POST_KEY_WAKE_DELAY);
            let mut woke_via_post_key = false;
            // The capture worker notifies on changed pane content.
            let preview_wake = self.home.preview_wake.clone();
            let mut woke_via_preview = false;

            // True for a preview too: it streams into the pane.
            let embedded_mounted = self.home.structured_preview.is_some();

            // All event sources are polled cooperatively via tokio::select!.
            // This ensures signal futures actually get scheduled (fixing #608
            // defect 1), and that EOF from a dead tty is detected (defect 2).
            tokio::select! {
                event = self.event_stream.as_mut().expect("event_stream missing").next() => {
                    self.home.apply_session_feed();
                    if self.home.sidebar_source != crate::tui::session_feed::SidebarSource::Daemon {
                        match event {
                            Some(Ok(Event::Key(key)))
                                if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                                #[cfg(feature = "e2e-tests")]
                                if key.code == KeyCode::F(12) && std::env::var_os("AOE_E2E_INPUT_BARRIER").is_some() {
                                    self.draw(terminal)?;
                                    e2e_render_ack(false)?;
                                    continue;
                                }
                                match (key.code, key.modifiers) {
                                    (KeyCode::Char('q'), KeyModifiers::NONE)
                                    | (KeyCode::Char('c'), KeyModifiers::CONTROL) => break,
                                    (KeyCode::Char('r'), KeyModifiers::NONE)
                                        if self.home.sidebar_source == crate::tui::session_feed::SidebarSource::Disconnected => {
                                        self.home.connect_runtime();
                                    }
                                    _ => {}
                                }
                            }
                            Some(Ok(Event::Resize(_, _))) => {}
                            None => break,
                            _ => continue,
                        }
                        self.draw(terminal)?;
                        continue;
                    }
                    match event {
                        Some(Ok(Event::Key(key))) => {
                            // Only act on key-down / auto-repeat. Terminals that
                            // report release events (Windows console always does;
                            // kitty-protocol terminals do when enhancement flags are
                            // on) would otherwise deliver a Release for every press
                            // and double-fire every handler, so a toggle like `i`
                            // (hide the info header) nets to zero and "won't hide".
                            // The acp and remote-home loops already filter this;
                            // the home loop has to as well.
                            if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                                continue;
                            }
                            crate::session::write_tui_activity();
                            // Paste-burst detector for VoiceInk + Mosh ergonomics.
                            // Mosh strips bracketed-paste markers, so pasted
                            // dictation arrives as a stream of individual KeyEvents
                            // that would otherwise fire home-view shortcuts (Q=quit,
                            // N=new, X=stop, D=delete, ...). Look-ahead-poll the
                            // event stream with a short inter-key timeout; if
                            // PASTE_BURST_MIN_LEN printable chars accumulate, route
                            // through handle_paste instead of dispatching them
                            // individually. Below the threshold we replay the
                            // captured keys as normal events.
                            //
                            // Only fire when home accepts paste routing
                            // (`wants_paste_burst`). Non-paste-aware dialogs
                            // — command palette, profile picker, projects,
                            // info, etc. — capture text via `handle_key`
                            // only; bursting through them strands the input
                            // in `pending_paste` and leaves the dialog empty.
                            // CI caught this regression with e2e harnesses
                            // that type fast enough to trip the burst.
                            if self.home.wants_paste_burst() && Self::is_burst_candidate(&key) {
                                let first_char = Self::burst_char_for(&key)
                                    .expect("is_burst_candidate guarantees burst_char_for returns Some");
                                let mut burst_str = String::new();
                                burst_str.push(first_char);
                                let mut burst_keys: Vec<KeyEvent> = vec![key];
                                let mut deferred: Option<Event> = None;
                                loop {
                                    let next = tokio::time::timeout(
                                        Duration::from_millis(PASTE_BURST_INTER_KEY_MS),
                                        self.event_stream.as_mut().expect("event_stream missing").next(),
                                    ).await;
                                    match next {
                                        // Ignore key-release / non-press events mid-burst, same
                                        // gate as the arm entry. On terminals that report releases
                                        // they would otherwise be taken as burst chars (doubling the
                                        // pasted text) or stashed as the deferred key.
                                        Ok(Some(Ok(Event::Key(k))))
                                            if !matches!(
                                                k.kind,
                                                KeyEventKind::Press | KeyEventKind::Repeat
                                            ) => {}
                                        Ok(Some(Ok(Event::Key(k)))) if Self::is_burst_candidate(&k) => {
                                            if let Some(c) = Self::burst_char_for(&k) {
                                                burst_str.push(c);
                                                burst_keys.push(k);
                                            }
                                        }
                                        Ok(Some(Ok(other))) => {
                                            deferred = Some(other);
                                            break;
                                        }
                                        _ => break,
                                    }
                                }
                                if burst_keys.len() >= PASTE_BURST_MIN_LEN
                                    && !Self::is_auto_repeat_burst(&burst_keys)
                                {
                                    // Peel a trailing Enter so the dialog's
                                    // plain-Enter Submit branch still fires.
                                    // Embedded mid-burst Enters stay as '\n'
                                    // in the paste text (the original reason
                                    // Enter is a burst candidate).
                                    let (paste_text, trailing_enter) =
                                        Self::split_trailing_enter(&burst_str, &burst_keys);
                                    if !paste_text.is_empty() {
                                        tracing::debug!(target: "tui.input",
                                            "paste-burst: routed {} chars via handle_paste (chars={:?})",
                                            paste_text.len(), paste_text
                                        );
                                        // An ACTIVE structured view owns text
                                        // input: the burst belongs to its
                                        // composer, same as a real Paste
                                        // event. A merely-mounted preview
                                        // must not eat it.
                                        if let Some(view) = self
                                            .home
                                            .structured_preview
                                            .as_mut()
                                            .filter(|v| v.is_active())
                                        {
                                            if let Err(e) = view
                                                .handle_event(Event::Paste(paste_text.clone()))
                                                .await
                                            {
                                                self.close_embedded_structured();
                                                self.update_status =
                                                    Some(UpdateStatus::transient(format!(
                                                        "structured view: {e}"
                                                    )));
                                            }
                                        } else {
                                            self.home.handle_paste(&paste_text);
                                        }
                                    }
                                    if let Some(enter) = trailing_enter {
                                        if !self.should_quit {
                                            self.handle_key(enter, terminal).await?;
                                        }
                                    }
                                } else {
                                    for k in burst_keys {
                                        self.handle_key(k, terminal).await?;
                                        if self.should_quit { break; }
                                    }
                                }
                                if !self.should_quit {
                                    if let Some(evt) = deferred {
                                        match evt {
                                            Event::Key(k) => { self.handle_key(k, terminal).await?; }
                                            Event::Paste(text) => { self.home.handle_paste(&text); }
                                            Event::Resize(_, _) => { terminal.autoresize()?; self.needs_redraw = true; }
                                            // Mirror the non-burst Mouse arm: scroll wheel
                                            // events can land between burst chars on touch
                                            // devices (scroll-while-dictating). Forward
                                            // ScrollUp/Down to the home view's scroll hit
                                            // targets so they don't get silently dropped.
                                            Event::Mouse(mouse) => {
                                                let hit_list = self.home.hit_list(mouse.column, mouse.row);
                                                let hit_preview = self.home.hit_preview(mouse.column, mouse.row);
                                                let hit_diff = self.home.is_diff_open()
                                                    && self.home.hit_diff(mouse.column, mouse.row);
                                                // See the non-burst arm: settings takeover
                                                // makes the whole screen a scroll target.
                                                let hit_scroll_target = hit_diff
                                                    || hit_list
                                                    || hit_preview
                                                    || self.home.owns_wheel();
                                                match mouse.kind {
                                                    MouseEventKind::ScrollUp if hit_scroll_target => { self.home.handle_scroll_up(mouse.column, mouse.row); }
                                                    MouseEventKind::ScrollDown if hit_scroll_target => { self.home.handle_scroll_down(mouse.column, mouse.row); }
                                                    // Burst-deferred clicks update selection but can't
                                                    // execute an activation action mid-burst (it'd tear
                                                    // down and reattach the terminal while we're still
                                                    // draining keystrokes). A user double-clicking
                                                    // during dictation can click again after the burst
                                                    // ends.
                                                    MouseEventKind::Down(MouseButton::Left) => {
                                                        if self.home.handle_context_menu_click(mouse.column, mouse.row)
                                                            || self.home.handle_dialog_click(mouse.column, mouse.row)
                                                            || self.home.handle_sidebar_collapse_click(mouse.column, mouse.row)
                                                            || self.home.handle_diagnostics_click(mouse.column, mouse.row)
                                                        {
                                                        } else if self.home.handle_tips_badge_click(mouse.column, mouse.row) {
                                                            // Footer tips badge opened the overlay;
                                                            // drop any stale preview highlight, like
                                                            // the non-burst click path does.
                                                            let _ = self.home.clear_preview_selection();
                                                        } else if hit_list {
                                                            let action = self.home.handle_click(mouse.column, mouse.row);
                                                            if action.is_none() {
                                                                let _ = self.home.handle_empty_list_click(mouse.column, mouse.row);
                                                            }
                                                        }
                                                    }
                                                    MouseEventKind::Down(MouseButton::Right) if hit_list => { self.home.handle_right_click(mouse.column, mouse.row); }
                                                    MouseEventKind::Moved => { self.home.handle_hover(mouse.column, mouse.row); }
                                                    _ => {}
                                                }
                                            }
                                            _ => {}
                                        }
                                    }
                                }

                                if !self.needs_redraw {
                                    self.draw(terminal)?;
                                }
                                if self.should_quit {
                                    break;
                                }
                                continue;
                            }

                            self.handle_key(key, terminal).await?;

                            // Arm the post-key wake when the key was
                            // routed into live-send. We don't have an
                            // explicit signal from handle_key for that
                            // (it returns ()), but `live_send.is_some()`
                            // after the call is a good proxy: a key
                            // that EXITS live-send won't arm a wake,
                            // and keys outside live-send leave it None
                            // anyway since we never set it.
                            let live_after = self.home.live_send.is_some();
                            if live_after {
                                last_live_key_at = Some(std::time::Instant::now());
                            }

                            // Skip the immediate draw when:
                            //   - We're returning from tmux attach
                            //     (`needs_redraw` triggers a clear +
                            //     stale event drain on the next
                            //     iteration; drawing before the drain
                            //     wastes a frame and can flicker), OR
                            //   - We're inside live-send. The key was
                            //     queued to the worker but has NOT been
                            //     dispatched to tmux yet, so the home
                            //     view's preview cache is still stale.
                            //     Drawing now produces a frame
                            //     identical to the previous one
                            //     (ratatui's diff is empty) and then
                            //     the post-key wake fires ~15ms later
                            //     with fresh post-echo content.
                            //     Skipping the immediate draw avoids a
                            //     no-op paint that on non-sync-update
                            //     terminals can still emit cursor-move
                            //     bytes mid-frame.
                            if !self.needs_redraw && !live_after {
                                self.draw(terminal)?;
                            }

                            if self.should_quit {
                                break;
                            }
                            continue;
                        }
                        Some(Ok(Event::Mouse(mouse))) => {
                            if !matches!(mouse.kind, MouseEventKind::Moved) {
                                crate::session::write_tui_activity();
                            }
                            // Structured preview mouse routing is deliberately
                            // thin: the transcript is ordinary preview content,
                            // so drags fall through to the home view's own
                            // drag-select machinery (fed by the transcript
                            // geometry each render) and a double-click
                            // activates via `preview_double_click_action`,
                            // both identical to terminal previews. Only the
                            // wheel is claimed here (it scrolls the
                            // transcript, which home cannot do), plus the
                            // "clicked off the pane while entered" drop back
                            // to preview so sidebar clicks keep selecting.
                                let in_pane = self.home.structured_preview.is_some()
                                    && self.home.preview_pane_area.contains(
                                        ratatui::layout::Position::from((
                                            mouse.column,
                                            mouse.row,
                                        )),
                                    );
                                if in_pane
                                    && matches!(
                                        mouse.kind,
                                        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                                    )
                                {
                                    if let Some(view) = self.home.structured_preview.as_mut() {
                                        let _ = view.handle_event(Event::Mouse(mouse)).await;
                                    }
                                    if !self.needs_redraw {
                                        self.draw(terminal)?;
                                    }
                                    continue;
                                }
                                let active = self
                                    .home
                                    .structured_preview
                                    .as_ref()
                                    .is_some_and(|v| v.is_active());
                                if active
                                    && !in_pane
                                    && matches!(
                                        mouse.kind,
                                        MouseEventKind::Down(MouseButton::Left)
                                            | MouseEventKind::Down(MouseButton::Right)
                                    )
                                {
                                    if let Some(v) = self.home.structured_preview.as_mut() {
                                        v.deactivate();
                                    }
                                }
                                if active
                                    && in_pane
                                    && matches!(
                                        mouse.kind,
                                        MouseEventKind::Down(MouseButton::Left)
                                    )
                                    && !mouse.modifiers.contains(KeyModifiers::SHIFT)
                                {
                                    if let Some(view) = self.home.structured_preview.as_mut() {
                                        let _ = view.handle_event(Event::Mouse(mouse)).await;
                                    }
                                    if !self.needs_redraw {
                                        self.draw(terminal)?;
                                    }
                                    continue;
                                }
                            // Footer toolbar: a left-click on a button
                            // synthesizes its shortcut and routes it through
                            // the full key handler, so clicking behaves
                            // exactly like pressing the key (global handling,
                            // action dispatch, structured-view drain). The
                            // footer is a disjoint area from the list/preview/
                            // diff, so nothing else in this arm needs to run.
                            // This runs ahead of the dialog/context-menu click
                            // handlers, but that is not a hazard: when an
                            // overlay is open `footer_button_at` returns `None`
                            // (its `has_non_live_send_overlay()` guard), so a
                            // click can never fire a shortcut behind a modal.
                            if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
                                if let Some(key) =
                                    self.home.footer_button_at(mouse.column, mouse.row)
                                {
                                    let _ = self.home.clear_preview_selection();
                                    self.handle_key(key, terminal).await?;
                                    if !self.needs_redraw {
                                        self.draw(terminal)?;
                                    }
                                    if self.should_quit {
                                        break;
                                    }
                                    continue;
                                }
                            }
                            // A double-click on the preview pane opens/attaches
                            // the previewed session, the same as a sidebar
                            // double-click. Checked BEFORE forwarding so the
                            // agent doesn't swallow the second press; a single
                            // press records its timing here and falls through to
                            // the forward path below.
                            if let Some(action) = self.home.preview_double_click_action(
                                mouse.kind,
                                mouse.modifiers,
                                mouse.column,
                                mouse.row,
                            ) {
                                let _ = self.home.clear_preview_selection();
                                self.execute_action(action, terminal)?;
                                // Mirror the list double-click path: an acp
                                // session only stashes its id, so drain and open
                                // the structured view here too.
                                if let Some(session_id) =
                                    self.pending_structured_view_open.take()
                                {
                                    self.open_structured_view(&session_id).await?;
                                }
                                if self.should_quit {
                                    break;
                                }
                                if !self.needs_redraw {
                                    self.draw(terminal)?;
                                }
                                continue;
                            }
                            // A hyperlink under the pointer opens in the
                            // browser. aoe captures the mouse, so the host
                            // terminal never sees this click and its own URL
                            // matching cannot help; a plain press is therefore
                            // the gesture. Runs ahead of the mouse forward so a
                            // mouse-tracking agent doesn't swallow it, and skips
                            // on Shift, which everywhere else on the preview
                            // means "aoe stays out of the way".
                            if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
                                && !mouse.modifiers.contains(KeyModifiers::SHIFT)
                            {
                                if let Some(url) =
                                    self.home.preview_link_at(mouse.column, mouse.row)
                                {
                                    // Say what happened either way, and never claim
                                    // an open that did not happen: when no browser
                                    // the user could see is reachable, the
                                    // clipboard still reaches their machine.
                                    let status =
                                        match crate::tui::open_url::open_url(&url) {
                                            Ok(()) => format!("opened {url}"),
                                            Err(e) => {
                                                crate::tui::clipboard::copy_to_clipboard(&url);
                                                format!("{e}; copied {url}")
                                            }
                                        };
                                    self.home.flash_status(status);
                                    let _ = self.home.clear_preview_selection();
                                    // This press is spent; without forgetting
                                    // it, clicking the link again pairs into a
                                    // double-click and attaches the session.
                                    self.home.forget_preview_click();
                                    self.draw(terminal)?;
                                    continue;
                                }
                            }
                            // Mouse-tracking agent under the preview (live-send
                            // OR passive hover): forward the press / drag /
                            // release straight to it, exactly as a direct attach
                            // would, so its native selection / scroll works.
                            // Shift falls through to aoe's own preview text-
                            // selection. Consumes the event when it forwards.
                            if self.home.forward_mouse_to_preview(
                                mouse.kind,
                                mouse.modifiers,
                                mouse.column,
                                mouse.row,
                            ) {
                                if !self.needs_redraw {
                                    self.draw(terminal)?;
                                }
                                continue;
                            }
                            let hit_list = self.home.hit_list(mouse.column, mouse.row);
                            let hit_preview = self.home.hit_preview(mouse.column, mouse.row);
                            let hit_diff = self.home.is_diff_open()
                                && self.home.hit_diff(mouse.column, mouse.row);
                            // Full-screen overlays cover the stale list/preview rects.
                            let hit_scroll_target = hit_diff
                                || hit_list
                                || hit_preview
                                || self.home.owns_wheel();
                            // Left-click priority: context menu, dialog (the diff
                            // view included), sidebar toggle, diagnostics, tips
                            // badge, drag start, list row.
                            let click_action = if matches!(
                                mouse.kind,
                                MouseEventKind::Down(MouseButton::Left)
                            ) {
                                if self
                                    .home
                                    .handle_context_menu_click(mouse.column, mouse.row)
                                {
                                    // Click consumed by the context menu:
                                    // either dispatched an item (Rename /
                                    // Delete), kept the menu open (border
                                    // hit), or dismissed it (click outside).
                                    self.draw(terminal)?;
                                    None
                                } else if self.home.handle_dialog_click(mouse.column, mouse.row)
                                {
                                    // A modal swallowed the click — drop any
                                    // leftover preview highlight so it doesn't
                                    // linger behind / through the dialog.
                                    let _ = self.home.clear_preview_selection();
                                    // Intro dialog can queue a live theme
                                    // preview or a final pick on click; apply
                                    // it before redrawing so the next frame
                                    // already reflects the choice.
                                    if let Some(name) = self.home.take_pending_intro_theme() {
                                        self.set_theme(&name);
                                    }
                                    self.draw(terminal)?;
                                    None
                                } else if self.home.handle_sidebar_collapse_click(mouse.column, mouse.row)
                                    || self.home.handle_diagnostics_click(mouse.column, mouse.row)
                                    || self.home.handle_tips_badge_click(mouse.column, mouse.row)
                                {
                                    let _ = self.home.clear_preview_selection();
                                    self.draw(terminal)?;
                                    None
                                } else if self
                                    .home
                                    .handle_drag_start(mouse.column, mouse.row)
                                {
                                    // handle_drag_start already overwrote the
                                    // selection if it started a PreviewSelect;
                                    // a fresh ListDivider drag is unrelated to
                                    // the highlight and should drop it.
                                    if !self.home.is_preview_select_dragging() {
                                        let _ = self.home.clear_preview_selection();
                                    }
                                    None
                                } else if hit_list {
                                    let _ = self.home.clear_preview_selection();
                                    let action = self
                                        .home
                                        .handle_click(mouse.column, mouse.row);
                                    // A click inside the list area that
                                    // didn't resolve to a row (empty space
                                    // below the last session) opens the
                                    // new-session dialog, mirroring `n`.
                                    if action.is_none() {
                                        let _ = self
                                            .home
                                            .handle_empty_list_click(mouse.column, mouse.row);
                                    }
                                    self.draw(terminal)?;
                                    action

                                } else if self.home.clear_preview_selection() {
                                    // A click on no surface at all still
                                    // dismisses a finalized highlight, and
                                    // nothing below repaints for a bare
                                    // Down(Left), so draw the clear here.
                                    self.draw(terminal)?;
                                    None
                                } else {
                                    None
                                }
                            } else {
                                None
                            };
                            let handled = match mouse.kind {
                                MouseEventKind::ScrollUp if hit_scroll_target => {
                                    self.home.handle_scroll_up(mouse.column, mouse.row)
                                }
                                MouseEventKind::ScrollDown if hit_scroll_target => {
                                    self.home.handle_scroll_down(mouse.column, mouse.row)
                                }
                                // Drag(Left) without a matching drag_state
                                // is a no-op inside the handler; we don't
                                // need a separate guard here.
                                MouseEventKind::Drag(MouseButton::Left) => {
                                    self.home.handle_drag_move(mouse.column, mouse.row)
                                }
                                MouseEventKind::Up(MouseButton::Left) => {
                                    // Finalize the drag here, but defer the
                                    // clipboard write until after the next
                                    // draw: the renderer captures cell text
                                    // while the buffer is still populated
                                    // (ratatui resets the back buffer on
                                    // every frame, so reading post-draw
                                    // sees empty cells).
                                    self.home.handle_drag_end()
                                }
                                // Right-click opens the sidebar context menu
                                // (Rename / Delete) for the clicked row.
                                // hit_list is the only place it makes sense
                                // today; other surfaces fall through.
                                MouseEventKind::Down(MouseButton::Right) if hit_list => {
                                    self.home.handle_right_click(mouse.column, mouse.row)
                                }
                                // Moved events are dispatched unconditionally
                                // (no `hit_list` guard) so the handler can
                                // clear the hover state the moment the
                                // cursor leaves the list, even when the new
                                // position lands on the preview or border.
                                MouseEventKind::Moved => {
                                    // Bare motion over the preview is also
                                    // reported to a hover-capable (any-event
                                    // tracking) agent so its own hover UI
                                    // works like a direct attach. It never
                                    // consumes the event: aoe's hover below
                                    // still runs, and no aoe redraw is
                                    // needed (the capture worker picks up
                                    // the agent's repaint).
                                    self.home
                                        .forward_hover_to_preview(mouse.column, mouse.row);
                                    // Route hover to the diff view's
                                    // file list when one is open AND
                                    // the mouse is over it; that's an
                                    // OR with the home view's own hover
                                    // (which already covers list +
                                    // overlay dialogs).
                                    let mut changed =
                                        self.home.handle_hover(mouse.column, mouse.row);
                                    // Show where a link goes while the pointer
                                    // rests on it: the click opens without
                                    // confirmation, so this is the only look
                                    // the user gets before committing.
                                    changed |= self
                                        .home
                                        .update_hovered_link(mouse.column, mouse.row);
                                    if hit_diff {
                                        changed |= self
                                            .home
                                            .handle_diff_hover(mouse.column, mouse.row);
                                    }
                                    if let Some(view) = self.home.structured_preview.as_mut() {
                                        changed |= view.handle_hover(mouse.column, mouse.row);
                                    }
                                    changed
                                }
                                _ => false,
                            };
                            if handled {
                                self.draw(terminal)?;
                            }
                            // After the draw that paints a freshly-finalized
                            // preview selection, the renderer has captured
                            // the cell text into `preview_copy_text`. Drain
                            // it and write to the user's clipboard.
                            if let Some(text) = self.home.take_preview_copy_text() {
                                crate::tui::clipboard::copy_to_clipboard(&text);
                            }
                            if let Some(action) = click_action {
                                self.execute_action(action, terminal)?;
                                // Mirror the handle_key path: Action::OpenStructuredView
                                // only stashes the id in `pending_structured_view_open`
                                // because the acp view needs async
                                // EventStream access that the sync
                                // `execute_action` can't lend. Drain here so a
                                // double-click on an acp session actually
                                // opens it.
                                if let Some(session_id) = self.pending_structured_view_open.take() {
                                    self.open_structured_view(&session_id).await?;
                                }
                            }
                            // Drain any Action stashed by a modal-dialog
                            // click (e.g. clicking `[Yes]` on a stop or
                            // quit confirm). The keyboard path returns
                            // these through handle_key; the click path
                            // can't, so it stashes them here.
                            if let Some(action) = self.home.pending_dialog_click_action.take() {
                                self.execute_action(action, terminal)?;
                                // A [Yes] click on the switch-view confirm
                                // stashes the switch; run it now, since this
                                // click path never reaches the key-path drain.
                                if let Some(session_id) = self.pending_view_switch.take() {
                                    self.perform_view_switch(&session_id, terminal).await;
                                }
                                // Same for a [Yes] click on the start-daemon
                                // confirm from a structured-view open.
                                if let Some(session_id) = self.pending_daemon_start_open.take() {
                                    self.start_daemon_then_open(&session_id, terminal).await;
                                }
                                // Same for an "Auto-name now" palette/menu click.
                                if let Some(session_id) = self.pending_smart_rename.take() {
                                    self.perform_smart_rename(&session_id).await;
                                }
                            }
                            continue;
                        }
                        Some(Ok(Event::Paste(text))) => {
                            crate::session::write_tui_activity();
                            // An ACTIVE structured view owns pasted text (it
                            // goes to its composer, same as the full-screen
                            // view). A merely-mounted preview must NOT eat
                            // it: the user is driving the home screen, and a
                            // paste belongs to whatever home surface is up.
                            if let Some(view) = self
                                .home
                                .structured_preview
                                .as_mut()
                                .filter(|v| v.is_active())
                            {
                                if let Err(e) = view.handle_event(Event::Paste(text)).await {
                                    self.close_embedded_structured();
                                    self.update_status = Some(UpdateStatus::transient(format!(
                                        "structured view: {e}"
                                    )));
                                }
                                self.draw(terminal)?;
                                continue;
                            }
                            self.home.handle_paste(&text);

                            self.draw(terminal)?;

                            continue;
                        }
                        Some(Ok(Event::Resize(_, _))) => {
                            // Soft keyboard slides up/down on iPad/iPhone Mosh
                            // (and ordinary terminal resizes) emit Resize. The
                            // catch-all below would silently drop them, leaving
                            // the screen mid-stale until the next refresh tick.
                            // Redraw now so viewport-driven layout
                            // (responsive::dialog_width, STACKED_BREAKPOINT,
                            // etc.) re-evaluates; ratatui's draw() autoresizes
                            // internally before rendering.
                            self.draw(terminal)?;
                            continue;
                        }
                        Some(Ok(_)) => {}
                        Some(Err(e)) => {
                            // IO error reading from the terminal (broken pipe,
                            // EOF, etc.) means the tty is gone. Exit cleanly
                            // instead of spinning (#608 defect 2).
                            tracing::info!(target: "tui.input", "Terminal event stream error, exiting: {}", e);
                            self.should_quit = true;
                            break;
                        }
                        None => {
                            // EventStream ended (EOF on stdin). The terminal is
                            // gone; exit instead of busy-looping (#608 defect 2).
                            tracing::info!(target: "tui.input", "Terminal event stream ended (EOF), exiting");
                            self.should_quit = true;
                            break;
                        }
                    }
                }
                // Embedded structured view: pump one daemon-side event
                // (ws frame, plugin snapshot, path roots). `next_event`
                // is cancel-safe (channel awaits only); the apply below
                // runs in the arm body where it can no longer be raced,
                // so a mid-replay cancellation cannot corrupt the state.
                ev = async {
                        self.home.structured_preview
                            .as_mut()
                            .expect("guarded by embedded_mounted")
                            .next_event()
                            .await
                }, if embedded_mounted => {
                        if let Some(view) = self.home.structured_preview.as_mut() {
                            view.apply_event(ev).await;
                        }
                        self.draw(terminal)?;
                }
                _ = refresh_interval.tick() => {}
                _ = preview_wake.notified() => {
                    // The capture worker produced fresh content. Repaint so
                    // it shows; an idle pane never fires this, so the home
                    // view stays as quiet as before when nothing changes.
                    woke_via_preview = true;
                }
                _ = async {
                    match post_key_deadline {
                        Some(at) => tokio::time::sleep_until(at.into()).await,
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    // Targeted refresh ~15ms after a live-send key,
                    // catching the agent's echo before the next ticker.
                    woke_via_post_key = true;
                    last_live_key_at = None;
                }
                _ = async {
                    #[cfg(unix)]
                    match sighup {
                        Some(ref mut s) => { s.recv().await; }
                        None => { std::future::pending::<()>().await; }
                    }
                    #[cfg(not(unix))]
                    std::future::pending::<()>().await;
                } => {
                    tracing::info!(target: "tui.input", "Received SIGHUP, exiting");
                    self.should_quit = true;
                    break;
                }
                _ = async {
                    #[cfg(unix)]
                    match sigterm {
                        Some(ref mut s) => { s.recv().await; }
                        None => { std::future::pending::<()>().await; }
                    }
                    #[cfg(not(unix))]
                    std::future::pending::<()>().await;
                } => {
                    tracing::info!(target: "tui.input", "Received SIGTERM, exiting");
                    self.should_quit = true;
                    break;
                }
                _ = async {
                    #[cfg(unix)]
                    match sigint {
                        Some(ref mut s) => { s.recv().await; }
                        None => { std::future::pending::<()>().await; }
                    }
                    #[cfg(not(unix))]
                    std::future::pending::<()>().await;
                } => {
                    tracing::info!(target: "tui.input", "Received SIGINT, exiting");
                    self.should_quit = true;
                    break;
                }
            }

            // Periodic refreshes (only when no input pending).
            //
            // `needs_full_refresh` separately tracks whether anything
            // other than the live-send ticker/post-key wake wants a
            // refresh; on those flags the cool-down at the bottom of
            // the loop is bypassed so deterministic signals (status
            // updates, dialog ticks) get painted right away.
            let mut refresh_needed = false;
            let mut needs_full_refresh = false;

            // A closed flash window needs exactly one repaint to clear the
            // row; the loop already wakes on the ticker, so this costs a
            // single frame rather than polling.
            needs_full_refresh |= self.home.expire_status_flash();

            // Continuous edge auto-scroll for a preview drag-select. The
            // mouse-event arm `continue`s above, so this runs on the
            // ~33ms ticker (and other wakes): while the cursor is held at
            // the pane edge, scroll one line and extend the selection so a
            // single drag can grab more than a page without depending on
            // mouse movement to fire events. No-op unless a drag is live
            // and the pointer sits at the edge.
            //
            // Request a normal (diffed) redraw via `refresh_needed`, NOT
            // `needs_redraw`: the latter forces a `clear_terminal` at the
            // top of the loop, and clearing every ticker frame while the
            // scroll runs strobes the screen blank-then-repaint. The diffed
            // draw at the bottom of the loop repaints smoothly.
            needs_full_refresh |= self.home.tick_preview_autoscroll();

            // Dwell-to-read: a session kept selected (list in the foreground)
            // for a few seconds counts as read and clears its unread marker.
            if self.home.tick_unread_dwell(std::time::Instant::now()) {
                refresh_needed = true;
            }

            // Banner changes invalidate the preview-only layout. Every poll must run.
            if self.poll_update_check()
                | self.poll_update_status()
                | self.poll_image_update_check()
                | self.poll_image_pull_status()
            {
                self.needs_redraw = true;
                needs_full_refresh = true;
            }

            if last_metrics_sample.elapsed() >= METRICS_SAMPLE_INTERVAL {
                self.home.request_metrics_refresh();
                last_metrics_sample = std::time::Instant::now();
            }

            // A new sample only repaints the strip, so a diffed redraw is
            // enough; no full clear.
            if self.home.apply_metrics_updates() {
                refresh_needed = true;
            }

            needs_full_refresh |= self.home.apply_session_feed();
            let had_native_error = self.home.info_dialog.is_some();
            if let Some(ready) = self.home.take_native_attachment() {
                use crate::tui::home::panes::PaneIntent;
                match ready.intent {
                    PaneIntent::Attach => self.attach_native_target(ready, terminal)?,
                    PaneIntent::LiveSend(target) => {
                        // The toast that covered the revive settles with the
                        // receipt, exactly as the blocking path used to.
                        self.update_status = None;
                        match self.home.enter_live_send_with(
                            &ready.id,
                            &ready.tmux_name,
                            target,
                            ready.lease,
                        ) {
                            Ok(()) => self.draw(terminal)?,
                            Err(()) => self.draw(terminal)?,
                        }
                    }
                    PaneIntent::Send { message, target } => {
                        self.update_status = None;
                        self.home.finish_send(
                            &ready.id,
                            &ready.tmux_name,
                            target,
                            &message,
                            &ready.lease,
                        );
                    }
                }
                needs_full_refresh = true;
            }
            needs_full_refresh |= !had_native_error && self.home.info_dialog.is_some();
            needs_full_refresh |= self.home.apply_structured_approval_results();

            needs_full_refresh |= self.home.apply_restart_results();

            let store_move = self.home.poll_store_move();
            needs_full_refresh |= store_move.changed;
            if let Some(action) = store_move.resume {
                self.execute_action(action, terminal)?;
                needs_full_refresh = true;
            }

            if let Some(session_id) = self.home.apply_creation_results() {
                self.dispatch_new_session_attach(&session_id, terminal)?;
                // A structured session routes the post-create attach into
                // `pending_structured_view_open`; drain it here (this tick
                // path sits outside the key/click drains).
                if let Some(sid) = self.pending_structured_view_open.take() {
                    self.open_structured_view(&sid).await?;
                }
                needs_full_refresh = true;
            }

            needs_full_refresh |= self.home.tick_dialog();

            // Fade the settings "Settings saved" toast once its window passes,
            // even if the user has stopped typing. Fires at most once per save,
            // so a full refresh here is free.
            needs_full_refresh |= self.home.tick_settings_status();

            // Full/config reloads stay deferred during live-send to preserve input
            // policy and mouse-capture state.
            let live_idle = self.home.live_send.is_none();
            if take_config_refresh_kick(live_idle, &self.home.config_watch.dirty) {
                let result = self.home.try_refresh_from_config_watcher();
                handle_tick_reload_config(result, &mut self.home.reload_failure_state);
                if let Some(theme_name) = self.home.take_pending_watcher_theme() {
                    self.set_theme(&theme_name);
                }
                needs_full_refresh = true;
            }

            let heartbeat_due = last_disk_refresh.elapsed() >= DISK_REFRESH_INTERVAL;
            // Consume watcher notifications in every mode. The decision below routes
            // live-send ticks to the storage-only reload.
            let dirty = self
                .home
                .disk_watch
                .dirty
                .swap(false, std::sync::atomic::Ordering::Acquire);
            let refresh_plan =
                plan_disk_refresh(live_idle, heartbeat_due, dirty, full_heartbeat_deferred);
            full_heartbeat_deferred = refresh_plan.full_heartbeat_deferred;

            match refresh_plan.decision {
                DiskRefreshDecision::FullHeartbeat => {
                    let reload_result = self.home.reload();
                    let reload_ok = reload_result.is_ok();
                    handle_tick_reload_storage(reload_result, &mut self.home.reload_failure_state);
                    if reload_ok {
                        let profile = self.home.active_profile.as_deref().unwrap_or("default");
                        let mouse_capture_allowed = crate::session::resolve_config(profile)
                            .map(|c| crate::tui::mouse_capture_requested(&c.session))
                            .unwrap_or(self.mouse_capture_allowed);
                        if mouse_capture_allowed != self.mouse_capture_allowed {
                            self.mouse_capture_allowed = mouse_capture_allowed;
                            self.sync_mouse_capture(terminal)?;
                        }
                    }
                    last_disk_refresh = std::time::Instant::now();
                    needs_full_refresh = true;
                }
                DiskRefreshDecision::StorageOnly => {
                    let reload_result = self.home.reload_storage_only();
                    handle_tick_reload_storage(reload_result, &mut self.home.reload_failure_state);
                    if heartbeat_due {
                        last_disk_refresh = std::time::Instant::now();
                    }
                    needs_full_refresh = true;
                }
                DiskRefreshDecision::None => {}
            }

            needs_full_refresh |= self.home.try_present_reload_failure_dialog();

            needs_full_refresh |= self.home.try_clear_recovered_reload_dialog();

            // Another surface (web live view, another TUI) took the
            // size-owner lock: exit live mode and let its grid stand,
            // instead of the old silent fight where the next keystroke or
            // preview-rect jitter stole the lock back.
            needs_full_refresh |= self.home.poll_live_send_takeover();

            if last_heartbeat.elapsed() >= HEARTBEAT_INTERVAL {
                crate::session::write_tui_heartbeat();
                last_heartbeat = std::time::Instant::now();
            }

            if last_telemetry_snapshot.elapsed() >= telemetry_snapshot_interval {
                last_telemetry_snapshot = std::time::Instant::now();
                self.emit_telemetry_snapshot();
            }

            if last_presence_refresh.elapsed() >= PRESENCE_REFRESH_INTERVAL {
                last_presence_refresh = std::time::Instant::now();
                let count = crate::session::count_active_tuis(PRESENCE_FRESH_WINDOW);
                if count != self.home.active_tui_count {
                    self.home.active_tui_count = count;
                    refresh_needed = true;
                }
            }

            // Periodic update re-check (#1471). The startup spawn only fires
            // once per process; long-running TUI sessions would otherwise
            // silently miss releases that ship after the user attached. The
            // throttle gap keeps the per-iteration `get_update_settings()`
            // config-file read off the 20Hz hot path.
            if last_update_eval.elapsed() >= UPDATE_CHECK_THROTTLE_GAP {
                last_update_eval = std::time::Instant::now();
                let settings = get_update_settings();
                if should_spawn_periodic_update_check(
                    last_update_check.map(|t| t.elapsed()),
                    PERIODIC_RECHECK_INTERVAL,
                    self.update_rx.is_some(),
                    settings.update_check_mode.is_enabled(),
                ) {
                    self.spawn_update_check();
                    last_update_check = Some(std::time::Instant::now());
                }
            }

            // Spinners live in the sidebar, which live-send users aren't watching.
            if last_spinner_redraw.elapsed() >= SPINNER_REDRAW_INTERVAL
                && self.home.has_animated_sessions()
                && self.home.live_send.is_none()
            {
                last_spinner_redraw = std::time::Instant::now();
                needs_full_refresh = true;
            }

            // Preview-on-select: mount/drop the streaming transcript
            // preview to track the selected structured session (debounced).
            needs_full_refresh |= self.reconcile_structured_preview().await;

            // Same cadence keeps the embedded composer caret blinking.
            if let Some(view) = self.home.structured_preview.as_mut() {
                let toast_changed = view.tick();
                if toast_changed || last_spinner_redraw.elapsed() >= SPINNER_REDRAW_INTERVAL {
                    last_spinner_redraw = std::time::Instant::now();
                    needs_full_refresh = true;
                }
            }

            // In live-send, the 33ms ticker is the steady-state
            // refresh source; treat every tick as a refresh. The
            // post-key wake (`woke_via_post_key`) is the same signal
            // but on a deterministic ~15ms delay after each keystroke
            // so typing-echo latency doesn't have to wait for ticker
            // phase. Outside live-send, only the periodic checks
            // above and the capture-worker wake (`woke_via_preview`,
            // fired only when pane content actually changed) trigger a
            // refresh.
            refresh_needed |= needs_full_refresh
                || self.home.live_send.is_some()
                || woke_via_post_key
                || woke_via_preview;

            // Cool-down guard against double-painting in live-send.
            // The post-key wake and the ticker can fire within 1ms of
            // each other (key pressed 14ms before a ticker tick: post-
            // key wake fires at +15ms, ticker tick fires at +16ms),
            // which doubles up frame writes and produces visible
            // tearing on terminals without synchronized-update
            // support. Skip ticker-driven refreshes inside the
            // cool-down window unless this refresh was specifically
            // requested by something else (status update, post-key
            // wake, or the capture-worker wake). Preview wakes carry
            // genuinely new pane content (the worker dedups and only
            // fires on change), so they're a real frame to paint, not a
            // redundant repaint, and must bypass the cool-down like the
            // post-key wake does or live-send echo stalls to the ticker.
            if refresh_needed
                && self.home.live_send.is_some()
                && !woke_via_post_key
                && !woke_via_preview
                && !needs_full_refresh
                && last_refresh_at.is_some_and(|t| t.elapsed() < REFRESH_COOLDOWN)
            {
                refresh_needed = false;
            }

            if refresh_needed {
                // Always do a full draw in live-send. The
                // `draw_preview_only` snapshot-painting fast path was
                // landed in #1495 to cheapen `%output` wakes, but
                // (a) `%output` wakes no longer exist (control-mode
                // is gone), and (b) on terminals that don't support
                // synchronized-update escapes (Apple Terminal.app,
                // Mosh-with-prediction), the snapshot-then-overlay
                // pattern produced visible "drag" (the previous
                // frame's preview cells stayed on screen for a beat
                // while ratatui's diff caught up). Always-full-draw is
                // ~2-3ms more CPU per frame (rebuilding the sidebar
                // widget tree) but is uniformly clean across
                // terminals. Outside live-send the same path runs
                // when `refresh_needed`, so this is just collapsing
                // the conditional branch.
                self.draw(terminal)?;
                last_refresh_at = Some(std::time::Instant::now());
            }

            if self.should_quit {
                break;
            }
        }

        self.home.apply_restart_results();
        self.home.cleanup_pending_creation();

        // Bounded and deduped so a dead endpoint or an unchanged launch-then-quit costs nothing.
        if let Some(snapshot) = self.build_telemetry_snapshot() {
            let reported = snapshot.session_creates_since_last_snapshot;
            let outcome = crate::telemetry::flush_snapshot_if_changed(snapshot).await;
            clear_reported_session_creates(reported, outcome);
        }

        Ok(())
    }

    /// Build a `usage_snapshot` from the current session list, or `None` when
    /// telemetry is not opted in. The TUI never hosts the web dashboard, so the
    /// `usage_seen` map is reported zeroed (a stable full key set), the
    /// per-client form-factor maps stay empty (and so omitted), and the
    /// structured-interaction counts are empty (the `aoe serve` daemon is the
    /// surface that tracks all of those). The create-trend counter carries the
    /// process-local `TUI_SESSION_CREATES` total, read *without reset* so a
    /// failed send retains it; the value is consumed only after a confirmed send
    /// (mirroring the serve deferred-clear).
    fn build_telemetry_snapshot(&self) -> Option<crate::telemetry::UsageSnapshot> {
        // Boundary snapshot: `build_usage_snapshot` takes `&[Instance]`
        // (shared API with the daemon caller in `src/server/serve_snapshot.rs`).
        let instances: Vec<crate::session::Instance> = self.home.instances().cloned().collect();
        crate::telemetry::build_usage_snapshot(
            crate::telemetry::Surface::Tui,
            &instances,
            crate::telemetry::usage_signals::zeroed(),
            reported_session_creates(),
            // The TUI hosts no server, so it has no auth or exposure mode.
            None,
            None,
            &crate::telemetry::StructuredInteractionCounts::default(),
        )
    }

    /// Build and send a snapshot, detached. No-op when not opted in. The send is
    /// awaited inside the spawned task only so the reported create count can be
    /// cleared after a confirmed send (the same await-and-clear discipline the
    /// serve periodic loop uses); the caller never blocks.
    fn emit_telemetry_snapshot(&self) {
        if let Some(snapshot) = self.build_telemetry_snapshot() {
            let reported = snapshot.session_creates_since_last_snapshot;
            tokio::spawn(async move {
                let outcome = if crate::telemetry::send_snapshot(snapshot).await {
                    crate::telemetry::SendOutcome::Sent
                } else {
                    crate::telemetry::SendOutcome::Failed
                };
                clear_reported_session_creates(reported, outcome);
            });
        }
    }

    fn render(&mut self, frame: &mut Frame) {
        let start = std::time::Instant::now();
        if self.update_status.as_ref().is_some_and(|s| s.is_expired()) {
            self.update_status = None;
        }
        let store_move_line = self.home.store_move_status_line();
        let status_text = self
            .update_status
            .as_ref()
            .map(|s| s.text.as_str())
            .or(store_move_line.as_deref());
        // Hidden while its own pull runs, so it can't re-render under the toast.
        let image_update = self
            .image_banner_active()
            .then_some(self.image_update.as_ref());
        // Reset so a frame that skips the preview path reports zero.
        self.home.preview_timings = Default::default();
        self.home.render(
            frame,
            frame.area(),
            &self.theme,
            self.update_info.as_ref(),
            status_text,
            image_update.flatten(),
        );
        // Sampled: only frames over the 16ms budget and live-send frames.
        let elapsed = start.elapsed();
        let in_live = self.home.live_send.is_some();
        if (elapsed.as_millis() > 16 || in_live)
            && tracing::enabled!(target: "tui.render", tracing::Level::TRACE)
        {
            let timings = self.home.preview_timings;
            tracing::trace!(
                target: "tui.render",
                frame_ms = elapsed.as_millis() as u64,
                frame_us = elapsed.as_micros() as u64,
                preview_apply_us = timings.apply.as_micros() as u64,
                parse_us = timings.parse.as_micros() as u64,
                live = in_live,
                width = frame.area().width,
                height = frame.area().height,
                "render frame sample",
            );
        }
    }

    /// Spawn an async update check, mirroring the brew-formula-lag
    /// suppression done at startup. Stores the receiver on `self.update_rx`
    /// so the main loop's `poll_update_check` picks up the result. Callers
    /// are responsible for gating on `update_check_mode.is_enabled()` and
    /// avoiding duplicate in-flight checks via `self.update_rx.is_none()`.
    fn spawn_update_check(&mut self) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.update_rx = Some(rx);
        tokio::spawn(async move {
            let version = env!("CARGO_PKG_VERSION");
            let mut result = check_for_update(version, false).await;
            // Homebrew formulas lag releases; hide the banner until `brew upgrade` can act.
            if let Ok(info) = &mut result {
                if info.available {
                    let target = info.latest_version.clone();
                    let actionable = tokio::task::spawn_blocking(move || {
                        crate::update::install::install_method_supports_target(&target)
                    })
                    .await
                    .unwrap_or(true);
                    if !actionable {
                        info.available = false;
                    }
                }
            }
            let _ = tx.send(result);
        });
    }

    /// Poll for update check result (non-blocking).
    /// Returns true if an update is available, was just received, and is
    /// not snoozed by a prior `dismissed_update_version`.
    fn poll_update_check(&mut self) -> bool {
        let (update_info, update_rx, received) =
            poll_update_receiver(self.update_rx.take(), self.update_info.take());
        self.update_info = update_info;
        self.update_rx = update_rx;

        if !received {
            return false;
        }

        let Some(info) = self.update_info.as_ref() else {
            return false;
        };

        // Already installed this version this session (auto or manual). The
        // running binary's compile-time `CARGO_PKG_VERSION` is stale until
        // the user restarts, so every periodic re-check (#1471) would
        // otherwise rediscover the same release: auto mode would loop the
        // installer, notify mode would re-show the banner. Skip both.
        if self.last_installed_version_in_session.as_deref() == Some(info.latest_version.as_str()) {
            tracing::info!(
                target: "update.dedup",
                version = %info.latest_version,
                "skipping: already installed this version this session, restart aoe to use it"
            );
            self.update_info = None;
            return false;
        }

        // Auto mode installs in the background; the new binary runs on next launch.
        if crate::session::get_update_settings()
            .update_check_mode
            .auto_installs()
        {
            self.maybe_kick_off_auto_install(info.latest_version.clone());
            self.update_info = None;
            return false;
        }

        // Notify mode: honor the per-version snooze. A newer release
        // clears the snooze automatically because the latest_version
        // string no longer matches.
        if self.dismissed_update_version.as_deref() == Some(info.latest_version.as_str()) {
            self.update_info = None;
            return false;
        }

        true
    }

    /// Whether the user runs sandboxed sessions, so a docker-image banner is
    /// worth surfacing. True when sandbox is on by default or any current
    /// session is sandboxed; otherwise we skip the registry check entirely.
    fn sandbox_in_use(&self) -> bool {
        if Config::load_or_warn().sandbox.enabled_by_default {
            return true;
        }
        self.home.instances().any(|i| i.is_sandboxed())
    }

    /// Is the sandbox-image banner the one currently shown? It sits below the
    /// app-update banner and transient toast, so it only owns the `u`/Ctrl+x
    /// keys when neither of those is up, and it must stay hidden while a pull it
    /// already started is still running (otherwise it re-arms `u` into the "pull
    /// already in progress" no-op, #2072).
    fn image_banner_active(&self) -> bool {
        should_show_image_banner(
            self.image_update.is_some(),
            self.update_info.is_some(),
            self.update_status.is_some(),
            self.image_pull_rx.is_some(),
        )
    }

    /// Spawn the background sandbox-image staleness check. Mirrors
    /// `spawn_update_check`: the result lands on `image_update_rx` for the
    /// main loop's `poll_image_update_check` to pick up.
    fn spawn_image_update_check(&mut self) {
        if self.image_update_rx.is_some() {
            return;
        }
        let image = Config::load_or_warn().sandbox.default_image.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.image_update_rx = Some(rx);
        tokio::spawn(async move {
            let result = crate::containers::image_update::check_for_image_update(&image).await;
            let _ = tx.send(result);
        });
    }

    /// Poll the image-update check (non-blocking). Returns true when a fresh,
    /// non-snoozed update just arrived and the banner should show.
    fn poll_image_update_check(&mut self) -> bool {
        let Some(mut rx) = self.image_update_rx.take() else {
            return false;
        };
        match rx.try_recv() {
            Ok(Ok(Some(update))) => {
                // Honor the per-digest snooze; a newer image clears it
                // automatically because its digest no longer matches.
                if self.dismissed_image_digest.as_deref() == Some(update.remote_digest.as_str()) {
                    return false;
                }
                self.image_update = Some(update);
                true
            }
            Ok(Ok(None)) => false,
            Ok(Err(e)) => {
                tracing::debug!(target: "containers.image_update", error = %e, "image update check failed");
                false
            }
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
                self.image_update_rx = Some(rx);
                false
            }
            Err(tokio::sync::oneshot::error::TryRecvError::Closed) => false,
        }
    }

    /// Kick off a `docker pull` of the sandbox image after the user accepts the
    /// banner's confirm dialog. The blocking pull runs on a std thread (the
    /// runtime call shells out); the result promotes into a transient toast.
    fn spawn_image_pull(&mut self, image: String) {
        if self.image_pull_rx.is_some() {
            return;
        }
        // Persistent: a pull outlives the transient window, and an expired toast
        // would let the banner re-render mid-pull.
        self.update_status = Some(UpdateStatus::persistent(format!("pulling {image}…")));
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.image_pull_rx = Some(rx);
        std::thread::spawn(move || {
            let result = crate::containers::get_container_runtime()
                .pull_image(&image)
                .map_err(anyhow::Error::from);
            let _ = tx.send(result);
        });
    }

    /// Poll the in-progress image pull. On success the banner clears (the local
    /// copy now matches the registry) and a confirmation toast shows. Returns
    /// true when the status line changed.
    fn poll_image_pull_status(&mut self) -> bool {
        let Some(mut rx) = self.image_pull_rx.take() else {
            return false;
        };
        match rx.try_recv() {
            Ok(Ok(())) => {
                self.image_update = None;
                self.set_status("sandbox image updated. New sessions will use it.");
                true
            }
            Ok(Err(e)) => {
                self.set_status(format!("image pull failed: {e}"));
                true
            }
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
                self.image_pull_rx = Some(rx);
                false
            }
            Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                self.set_status("image pull ended unexpectedly");
                true
            }
        }
    }

    /// Kick off a background install when `update_check_mode = "auto"` and a
    /// new release is detected. Tarball + writable parent is the only safe
    /// auto path: Homebrew expects the user to run `brew upgrade`, and a
    /// sudo-required tarball install can't prompt without a TTY. In every
    /// other case we silently no-op so the user can still run `aoe update`
    /// manually.
    fn maybe_kick_off_auto_install(&mut self, version: String) {
        use crate::update::install::{detect_install_method, perform_update, InstallMethod};

        // Defensive: if a prior auto- or manual update is still running,
        // do not start a second installer or overwrite `update_status_rx`.
        // Mirrors the guard in `Action::SpawnUpdate`.
        if self.update_status_rx.is_some() {
            tracing::info!(
                target: "update.auto",
                "auto mode skipped: update already in progress"
            );
            return;
        }

        let method = match detect_install_method() {
            Ok(m) => m,
            Err(e) => {
                tracing::info!(
                    target: "update.auto",
                    error = %e,
                    "auto mode skipped: install method detection failed"
                );
                return;
            }
        };
        let writable = match &method {
            InstallMethod::Tarball { binary_path } => {
                crate::update::install::parent_is_writable(binary_path)
            }
            _ => false,
        };
        if !writable {
            tracing::info!(
                target: "update.auto",
                ?method,
                "auto mode skipped: install method needs an interactive update"
            );
            return;
        }

        self.set_status(format!("auto-updating to v{version} in background…"));
        // Stash for `poll_update_status` to promote into
        // `last_installed_version_in_session` on confirmed success. Tracking
        // only on success preserves the user's ability to retry after a
        // failed install (transient network issue, disk full, etc.).
        self.pending_install_version = Some(version.clone());
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.update_status_rx = Some(rx);
        let handle = tokio::runtime::Handle::current();
        std::thread::spawn(move || {
            let result = handle.block_on(perform_update(&method, &version, None));
            let _ = tx.send(result);
        });
    }

    /// Poll the in-progress update task for completion.
    /// Returns true when the status line changed and a redraw is needed.
    fn poll_update_status(&mut self) -> bool {
        let Some(mut rx) = self.update_status_rx.take() else {
            return false;
        };
        match rx.try_recv() {
            Ok(Ok(())) => {
                // Promote the pending version into the per-session record so
                // the periodic re-check (#1471) stops surfacing this release.
                self.last_installed_version_in_session = self.pending_install_version.take();
                self.update_status = Some(UpdateStatus::persistent(
                    "update complete. Restart aoe to use the new version.".into(),
                ));
                true
            }
            Ok(Err(e)) => {
                // Clear pending so a retry is allowed.
                self.pending_install_version = None;
                self.set_status(format!("update failed: {e}"));
                true
            }
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
                self.update_status_rx = Some(rx);
                false
            }
            Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                self.pending_install_version = None;
                self.set_status("update task ended unexpectedly");
                true
            }
        }
    }

    /// Dispatch the confirmed update, choosing between a blocking suspend and a
    /// background tokio task based on whether the method requires sudo.
    fn spawn_update(
        &mut self,
        method: crate::update::install::InstallMethod,
        version: String,
        terminal: &mut Terminal<TuiBackend>,
    ) -> Result<()> {
        use crate::update::install::InstallMethod;

        let needs_sudo = matches!(
            &method,
            InstallMethod::Tarball { binary_path }
                if !crate::update::install::parent_is_writable(binary_path)
        );

        if matches!(method, InstallMethod::Homebrew) || needs_sudo {
            // Suspend the TUI so sudo's password prompt can use the terminal.
            self.set_status(format!("updating to v{version}…"));
            let method_clone = method.clone();
            let version_clone = version.clone();
            let result = self.with_raw_mode_disabled(terminal, move || {
                tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(async {
                        crate::update::install::perform_update(&method_clone, &version_clone, None)
                            .await
                    })
                })
            })?;
            match result {
                Ok(()) => {
                    // Record the successful manual install so the periodic
                    // re-check (#1471) stops re-surfacing this release.
                    self.last_installed_version_in_session = Some(version.clone());
                    self.update_status = Some(UpdateStatus::persistent(
                        "update complete. Restart aoe to use the new version.".into(),
                    ));
                }
                Err(e) => {
                    self.set_status(format!("update failed: {e}"));
                }
            }
        } else {
            // Background task for writable tarball installs.
            // `perform_update`'s future is !Send because its `on_progress` parameter is
            // `Option<&mut dyn FnMut(...)>` (no Send bound on the trait object), so
            // `tokio::spawn` won't accept it. A std::thread + Handle::block_on lets the
            // async I/O still use the existing tokio runtime while sidestepping the
            // Send constraint.
            self.set_status(format!("updating to v{version}…"));
            // Stash for `poll_update_status` to promote on confirmed success
            // (#1471). Mirrors the auto-install path.
            self.pending_install_version = Some(version.clone());
            let (tx, rx) = tokio::sync::oneshot::channel();
            self.update_status_rx = Some(rx);
            let handle = tokio::runtime::Handle::current();
            std::thread::spawn(move || {
                let result = handle.block_on(crate::update::install::perform_update(
                    &method, &version, None,
                ));
                let _ = tx.send(result);
            });
        }
        Ok(())
    }
}

/// Persist `app_state.dismissed_update_version` so the snooze (Ctrl+x on the
/// update banner) survives restarts. Errors are logged but never surfaced,
/// because losing the snooze is not worth pausing the event loop over.
fn persist_dismissed_update_version(version: Option<String>) {
    let result = update_app_state(|state| {
        state.dismissed_update_version = version;
    });
    if let Err(e) = result {
        tracing::warn!(
            target: "update.snooze",
            error = %e,
            "failed to persist dismissed_update_version"
        );
    }
}

/// Persist `app_state.dismissed_image_digest` so dismissing the sandbox-image
/// banner (Ctrl+x) survives restarts. Like the update snooze, failures are
/// logged but never surfaced.
fn persist_dismissed_image_digest(digest: Option<String>) {
    let result = update_app_state(|state| {
        state.dismissed_image_digest = digest;
    });
    if let Err(e) = result {
        tracing::warn!(
            target: "containers.image_update",
            error = %e,
            "failed to persist dismissed_image_digest"
        );
    }
}

/// Decide whether the main loop should spawn a fresh periodic update check.
/// Pulled out as a pure function so the throttle/in-flight/mode guards are
/// testable without driving the tokio runtime, the config file, or the
/// network. `elapsed = None` means no check has run yet this process, which
/// makes the first tick after the user enables update_check_mode mid-session
/// fire immediately rather than waiting up to `PERIODIC_RECHECK_INTERVAL`
/// from process launch.
fn should_spawn_periodic_update_check(
    elapsed: Option<Duration>,
    interval: Duration,
    rx_in_flight: bool,
    mode_enabled: bool,
) -> bool {
    if rx_in_flight || !mode_enabled {
        return false;
    }
    match elapsed {
        None => true,
        Some(e) => e >= interval,
    }
}

/// Polls the update receiver and returns the new state.
/// Returns (update_info, update_rx, was_update_received).
fn poll_update_receiver(
    rx: Option<tokio::sync::oneshot::Receiver<anyhow::Result<UpdateInfo>>>,
    current_info: Option<UpdateInfo>,
) -> (
    Option<UpdateInfo>,
    Option<tokio::sync::oneshot::Receiver<anyhow::Result<UpdateInfo>>>,
    bool,
) {
    if let Some(mut rx) = rx {
        match rx.try_recv() {
            Ok(result) => {
                if let Ok(info) = result {
                    if info.available {
                        return (Some(info), None, true);
                    }
                }
                (current_info, None, false)
            }
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
                (current_info, Some(rx), false)
            }
            Err(tokio::sync::oneshot::error::TryRecvError::Closed) => (current_info, None, false),
        }
    } else {
        (current_info, None, false)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiskRefreshDecision {
    FullHeartbeat,
    StorageOnly,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DiskRefreshPlan {
    decision: DiskRefreshDecision,
    full_heartbeat_deferred: bool,
}

fn take_config_refresh_kick(live_idle: bool, config_dirty: &std::sync::atomic::AtomicBool) -> bool {
    live_idle && config_dirty.swap(false, std::sync::atomic::Ordering::Acquire)
}

/// Pure refresh-policy decision. Storage-only refreshes remain eligible during
/// live-send, both for watcher notifications and as the periodic fallback. The
/// full heartbeat reload requires an idle live-send state.
fn decide_disk_refresh(live_idle: bool, heartbeat_due: bool, dirty: bool) -> DiskRefreshDecision {
    if live_idle && heartbeat_due {
        DiskRefreshDecision::FullHeartbeat
    } else if heartbeat_due || dirty {
        DiskRefreshDecision::StorageOnly
    } else {
        DiskRefreshDecision::None
    }
}

/// Preserve an overdue full heartbeat across storage-only timer resets, so it
/// runs on the first idle tick after live-send exits.
fn plan_disk_refresh(
    live_idle: bool,
    heartbeat_due: bool,
    dirty: bool,
    full_heartbeat_deferred: bool,
) -> DiskRefreshPlan {
    let decision = decide_disk_refresh(
        live_idle,
        heartbeat_due || (live_idle && full_heartbeat_deferred),
        dirty,
    );
    let full_heartbeat_deferred = match decision {
        DiskRefreshDecision::FullHeartbeat => false,
        DiskRefreshDecision::StorageOnly if !live_idle && heartbeat_due => true,
        _ => full_heartbeat_deferred,
    };
    DiskRefreshPlan {
        decision,
        full_heartbeat_deferred,
    }
}

/// Whether the sandbox-image banner should own the bottom row right now. It is
/// the lowest-priority banner, so it yields to the app-update banner and any
/// transient toast, and it stays hidden while a pull it kicked off is still
/// running. That last clause is the #2072 fix: without it the banner reappeared
/// the moment the "pulling…" toast cleared, redrawing itself on top of an
/// in-flight pull and re-arming `u` into the "pull already in progress" no-op.
/// Factored out of `App::image_banner_active` so the policy is unit-testable.
fn should_show_image_banner(
    has_image_update: bool,
    has_app_update: bool,
    has_status: bool,
    pull_in_flight: bool,
) -> bool {
    has_image_update && !has_app_update && !has_status && !pull_in_flight
}

/// Catches reload errors so the tick loop never propagates them. A
/// malformed `sessions.json` or `groups.json` written by a peer process
/// is logged, recorded in `ReloadFailureState` for one-shot dialog
/// surfacing, and the tick loop continues with the previous in-memory
/// state. The next successful reload clears the recorded failure.
fn handle_tick_reload_storage(
    result: anyhow::Result<()>,
    state: &mut crate::tui::home::ReloadFailureState,
) {
    if let Err(ref e) = result {
        tracing::warn!(
            target: "tui.file_watch",
            error = %e,
            "tick storage reload failed; preserving in-memory state, will retry on next tick"
        );
    }
    if state.record_storage(&result) {
        tracing::info!(
            target: "tui.file_watch",
            "storage reload recovered"
        );
    }
}

/// Tick-driven config reload errors must never propagate out of the
/// main loop AND must never silently flip safety-affecting settings to
/// defaults. A malformed `config.toml` written by a peer process would
/// otherwise either crash the TUI (if propagated) or silently disable
/// settings like `confirm_before_quit` (if applied as default). This
/// helper records the failure for one-shot dialog surfacing while
/// keeping the previous in-memory config intact.
fn handle_tick_reload_config(
    result: anyhow::Result<()>,
    state: &mut crate::tui::home::ReloadFailureState,
) {
    if let Err(ref e) = result {
        tracing::warn!(
            target: "tui.file_watch",
            error = %e,
            "tick config reload failed; preserving in-memory config, will retry on next tick"
        );
    }
    if state.record_config(&result) {
        tracing::info!(
            target: "tui.file_watch",
            "config reload recovered"
        );
    }
}

/// What a `q` key press at the home screen should do. Factored out of the
/// key handler so the quit policy is unit-testable.
#[derive(Debug, PartialEq, Eq)]
enum QuitIntent {
    /// Don't quit. Ctrl+Q lands here: it's reserved for exiting live-send
    /// mode and must never close aoe from the home view (#1569).
    Ignore,
    /// A session is mid-creation; confirm before cancelling it.
    ConfirmDuringCreation,
    /// Confirm-before-quit is enabled; show the quit confirmation.
    Confirm,
    /// Quit immediately.
    Quit,
}

fn quit_intent(
    modifiers: KeyModifiers,
    creation_pending: bool,
    confirm_before_quit: bool,
) -> QuitIntent {
    if modifiers.contains(KeyModifiers::CONTROL) {
        return QuitIntent::Ignore;
    }
    if creation_pending {
        return QuitIntent::ConfirmDuringCreation;
    }
    if confirm_before_quit {
        return QuitIntent::Confirm;
    }
    QuitIntent::Quit
}

#[cfg(feature = "e2e-tests")]
pub(crate) fn e2e_render_ack(initial: bool) -> Result<()> {
    let Some(path) = std::env::var_os("AOE_E2E_INPUT_BARRIER") else {
        return Ok(());
    };
    let sequence = if initial {
        0
    } else {
        std::fs::read_to_string(&path)?.parse::<u64>()? + 1
    };
    std::fs::write(path, sequence.to_string())?;
    crossterm::execute!(
        std::io::stdout(),
        crossterm::terminal::SetTitle(format!("aoe-e2e-{sequence}"))
    )?;
    Ok(())
}

impl App {
    async fn handle_key(
        &mut self,
        key: KeyEvent,
        terminal: &mut Terminal<TuiBackend>,
    ) -> Result<()> {
        #[cfg(feature = "e2e-tests")]
        if key.code == KeyCode::F(12) && std::env::var_os("AOE_E2E_INPUT_BARRIER").is_some() {
            self.draw(terminal)?;
            e2e_render_ack(false)?;
            return Ok(());
        }
        // An ACTIVE embedded view owns the keyboard; a mere preview does not.
        if self
            .home
            .structured_preview
            .as_ref()
            .is_some_and(|v| v.is_active())
        {
            let result = self
                .home
                .structured_preview
                .as_mut()
                .expect("checked is_some above")
                .handle_event(crossterm::event::Event::Key(key))
                .await;
            match result {
                Ok(true) => {
                    if let Some(v) = self.home.structured_preview.as_mut() {
                        v.deactivate();
                    }
                }
                Ok(false) => {}
                Err(e) => {
                    self.close_embedded_structured();
                    self.set_status(format!("structured view: {e}"));
                }
            }
            return Ok(());
        }
        // Global keybindings
        match (key.code, key.modifiers) {
            // Ctrl+C belongs to the agent in live-send.
            (KeyCode::Char('c'), KeyModifiers::CONTROL) if !self.home.is_live_send_capturing() => {
                if self.home.is_creating_stub_selected() {
                    self.home.cancel_creation();
                    return Ok(());
                }
                if self.home.is_creation_pending() && !self.home.has_dialog() {
                    self.home.show_quit_during_creation_confirm();
                    return Ok(());
                }
                self.should_quit = true;
                return Ok(());
            }
            (KeyCode::Char('q'), modifiers) if !self.home.has_dialog() => {
                match quit_intent(
                    modifiers,
                    self.home.is_creation_pending(),
                    self.home.confirm_before_quit(),
                ) {
                    QuitIntent::Ignore => {}
                    QuitIntent::ConfirmDuringCreation => {
                        self.home.show_quit_during_creation_confirm();
                    }
                    QuitIntent::Confirm => {
                        self.home.show_quit_confirm();
                    }
                    QuitIntent::Quit => {
                        self.should_quit = true;
                    }
                }
                return Ok(());
            }
            // No `needs_redraw`: its full clear flashes the screen.
            (KeyCode::Char('x'), KeyModifiers::CONTROL)
                if (self.update_info.is_some()
                    || self.update_status.is_some()
                    || self.image_update.is_some())
                    && !self.home.has_dialog() =>
            {
                // The image banner is lowest priority, so Ctrl+x only dismisses
                // it when it's the one actually showing. Otherwise it targets
                // the app update / toast as before, leaving any pending image
                // update to surface once those clear.
                if self.image_banner_active() {
                    if let Some(update) = self.image_update.as_ref() {
                        let digest = update.remote_digest.clone();
                        self.dismissed_image_digest = Some(digest.clone());
                        persist_dismissed_image_digest(Some(digest));
                    }
                    self.image_update = None;
                    return Ok(());
                }
                if let Some(info) = self.update_info.as_ref() {
                    let v = info.latest_version.clone();
                    self.dismissed_update_version = Some(v.clone());
                    persist_dismissed_update_version(Some(v));
                }
                self.update_info = None;
                self.update_status = None;
                return Ok(());
            }
            // The image banner only shows without an app update, so `u` is free.
            (KeyCode::Char('u'), KeyModifiers::NONE)
                if self.image_banner_active() && !self.home.has_dialog() =>
            {
                if let Some(update) = self.image_update.as_ref() {
                    let image = update.image.clone();
                    self.home.prompt_pull_sandbox_image(image);
                }
                return Ok(());
            }
            _ => {}
        }
        if let Some(action) = self.home.handle_key(key, self.update_info.as_ref()) {
            self.execute_action(action, terminal)?;
        }

        // Drained after the key: `execute_action` may have just stashed them.
        if let Some(session_id) = self.pending_view_switch.take() {
            self.perform_view_switch(&session_id, terminal).await;
        }

        if let Some(session_id) = self.pending_daemon_start_open.take() {
            self.start_daemon_then_open(&session_id, terminal).await;
        }

        if let Some(session_id) = self.pending_structured_view_open.take() {
            self.open_structured_view(&session_id).await?;
        }

        if let Some(session_id) = self.pending_smart_rename.take() {
            self.perform_smart_rename(&session_id).await;
        }

        Ok(())
    }

    /// Run a stashed on-demand "Auto-name now" for a structured session: resolve
    /// the daemon and POST `/smart-rename`. The daemon forces past the disabled
    /// setting and runs the one-shot detached; the new title arrives over the
    /// structured-view WS and the file watcher refreshes the row, so the TUI
    /// mutates no session state itself. A no-daemon state surfaces as a
    /// transient status rather than failing the loop (#3039).
    async fn perform_smart_rename(&mut self, session_id: &str) {
        use crate::acp::client::{require_daemon, HttpClient, ManagerError};

        let title = self
            .home
            .get_instance(session_id)
            .map(|i| i.title.clone())
            .unwrap_or_default();

        let endpoint = match require_daemon().await {
            Ok(e) => e,
            Err(ManagerError::NoDaemonRunning(_)) => {
                self.set_status(
                    "Auto-name needs a running daemon; open the structured view first.",
                );
                return;
            }
            Err(e) => {
                self.set_status(format!("daemon unreachable: {e}"));
                return;
            }
        };
        let http = match HttpClient::new(endpoint) {
            Ok(h) => h,
            Err(e) => {
                self.set_status(format!("auto-name failed: {e}"));
                return;
            }
        };
        self.set_status(match http.smart_rename(session_id).await {
            Ok(()) => format!("auto-naming \"{title}\"…"),
            Err(e) => format!("auto-name failed: {e}"),
        });
    }

    /// Run a stashed view switch: resolve the daemon, POST the matching
    /// switch endpoint, and surface the outcome as a transient status.
    /// The daemon persists the flipped view and (re)spawns the worker /
    /// tears down the pane; the file watcher refreshes the row, so the
    /// TUI mutates no session state itself. Errors surface as status
    /// text rather than failing the app loop.
    ///
    /// When no daemon is running, a localhost one is started first: the
    /// user just confirmed a dialog that says the agent restarts under
    /// `aoe serve`, so the spawn is part of the consented action rather
    /// than a hidden side effect. `terminal` is borrowed to paint the
    /// "Starting…" status before the (up to several seconds) wait.
    async fn perform_view_switch(&mut self, session_id: &str, terminal: &mut Terminal<TuiBackend>) {
        use crate::acp::client::{require_daemon, HttpClient, ManagerError};

        let Some(inst) = self.home.get_instance(session_id) else {
            return;
        };
        let to_structured = !inst.is_structured();
        let title = inst.title.clone();

        let endpoint = match require_daemon().await {
            Ok(e) => e,
            Err(ManagerError::NoDaemonRunning(_)) => {
                self.set_status("Starting local daemon for the view switch…");
                let _ = self.draw(terminal);
                match crate::tui::dialogs::start_local_daemon_and_wait().await {
                    Ok(e) => e,
                    Err(e) => {
                        // The log-tail hint is multi-line; a status is one row.
                        let first = e.lines().next().unwrap_or("unknown error");
                        self.set_status(format!("view switch failed: {first}"));
                        return;
                    }
                }
            }
            Err(e) => {
                self.set_status(format!("daemon unreachable: {e}"));
                return;
            }
        };
        let http = match HttpClient::new(endpoint) {
            Ok(h) => h,
            Err(e) => {
                self.set_status(format!("view switch failed: {e}"));
                return;
            }
        };
        let result = if to_structured {
            http.acp_enable(session_id).await
        } else {
            http.acp_disable(session_id).await
        };
        self.set_status(match result {
            Ok(()) if to_structured => format!("\"{title}\" switched to the structured view"),
            Ok(()) => format!("\"{title}\" switched to the terminal view"),
            Err(e) => format!("view switch failed: {e}"),
        });
    }

    /// Enter (activate) the structured view for `session_id`: it takes
    /// the keyboard so the composer works. Usually the view is already
    /// mounted as a preview (selecting the row mounts it), so this just
    /// flips it active. If it isn't mounted (e.g. the daemon was down
    /// when selected), connect now; and with no daemon at all, offer to
    /// start a localhost one (the Yes path resumes through
    /// `start_daemon_then_open`).
    async fn open_structured_view(&mut self, session_id: &str) -> Result<()> {
        use crate::acp::client::{require_daemon, ManagerError};

        // Archived rows render a placeholder; an active view would capture keys invisibly.
        if self
            .home
            .get_instance(session_id)
            .is_some_and(|inst| inst.is_archived() || inst.is_trashed())
        {
            self.set_status(
                "This session is archived; restore it first to open the structured view",
            );
            return Ok(());
        }
        if self
            .home
            .structured_preview
            .as_ref()
            .is_some_and(|v| v.session_id() == session_id)
        {
            self.activate_embedded();
            self.drain_pending_paste_for_structured_view(session_id)
                .await;
            return Ok(());
        }
        match require_daemon().await {
            Ok(endpoint) => {
                self.connect_embedded_structured(endpoint, session_id).await;
                self.activate_embedded();
                self.drain_pending_paste_for_structured_view(session_id)
                    .await;
            }
            Err(ManagerError::NoDaemonRunning(_)) => {
                self.home.prompt_start_daemon_for_structured(session_id);
            }
            Err(e) => {
                self.set_status(format!("structured view: {e}"));
            }
        }
        Ok(())
    }

    /// Flip the mounted embedded view to interactive mode (exiting
    /// live-send first, since both own the preview pane and keyboard).
    fn activate_embedded(&mut self) {
        self.home.exit_live_send_if_active();
        if let Some(v) = self.home.structured_preview.as_mut() {
            v.activate();
        }
    }

    /// Consume the requested session's draft only if that session mounted.
    async fn drain_pending_paste_for_structured_view(&mut self, session_id: &str) {
        let Some(view) = self
            .home
            .structured_preview
            .as_mut()
            .filter(|view| view.session_id() == session_id)
        else {
            return;
        };
        if let Some(buf) = self
            .home
            .pending_paste_for_structured_view
            .remove(session_id)
        {
            view.paste_text_with_file_load(&buf).await;
        }
    }

    /// Mount the embedded view against a located daemon in preview
    /// (read-only) state. The caller activates it if the user is
    /// entering rather than just previewing.
    async fn connect_embedded_structured(
        &mut self,
        endpoint: crate::acp::client::DaemonEndpoint,
        session_id: &str,
    ) {
        use crate::tui::structured_view::embedded::EmbeddedView;

        match EmbeddedView::connect(endpoint, session_id).await {
            Ok(view) => {
                self.home.structured_preview = Some(view);
                self.preview_mount_pending = None;
            }
            Err(e) => {
                self.set_status(format!("structured view: {e}"));
            }
        }
    }

    /// Drop the embedded structured view and hand the preview pane
    /// back to the home screen. Dropping the view closes its WebSocket
    /// and ends the side-channel tasks (their senders error out).
    ///
    /// No `needs_redraw`: that forces a full `clear_terminal` on the next
    /// loop iteration, which blanks the whole screen for a frame (the
    /// reported Ctrl+Q flash). The home view repaints the same preview
    /// rect the structured view drew into, so the ordinary diffed draw
    /// covers it cleanly, the same way exiting live-send does.
    fn close_embedded_structured(&mut self) {
        self.home.structured_preview = None;
    }

    /// The Yes path of the "start a local daemon?" confirm: spawn a
    /// localhost daemon with visible feedback, wait for it to become
    /// healthy, then mount + enter the embedded structured view.
    async fn start_daemon_then_open(
        &mut self,
        session_id: &str,
        terminal: &mut Terminal<TuiBackend>,
    ) {
        self.set_status("Starting local daemon…");
        let _ = self.draw(terminal);
        match crate::tui::dialogs::start_local_daemon_and_wait().await {
            Ok(endpoint) => {
                self.update_status = None;
                self.connect_embedded_structured(endpoint, session_id).await;
                self.activate_embedded();
                // Preserve the captured draft until its requested view mounts.
                self.drain_pending_paste_for_structured_view(session_id)
                    .await;
            }
            Err(e) => {
                let first = e.lines().next().unwrap_or("unknown error");
                self.set_status(format!("daemon start failed: {first}"));
            }
        }
    }

    fn clear_preview_mount_pending(&mut self) {
        self.preview_mount_pending = None;
        self.home.structured_preview_pending = false;
    }

    /// Preview-on-select: keep a streaming structured-transcript preview
    /// mounted for the selected structured session. Debounced so rapid
    /// list navigation doesn't connect a socket per keystroke, and only
    /// while a daemon is already reachable (a down daemon leaves the
    /// "press Enter" placeholder). An active (entered) view is never
    /// disturbed. Returns true if the mount set changed (needs redraw).
    async fn reconcile_structured_preview(&mut self) -> bool {
        if self.home.sidebar_source != crate::tui::session_feed::SidebarSource::Daemon {
            self.clear_preview_mount_pending();
            return self.home.structured_preview.take().is_some();
        }
        // An entered view owns the selection and keyboard; leave it be,
        // but only while its session is still a live structured row AND
        // still the selected one. A peer (web, another aoe) can delete
        // the session, flip it to a terminal view, or archive it out
        // from under us, and a storage reload can move the selection;
        // an active view that no longer matches what the pane renders
        // would keep capturing every keystroke invisibly.
        if let Some(view) = self
            .home
            .structured_preview
            .as_ref()
            .filter(|view| view.is_active())
        {
            let still_valid =
                self.home.selected_structured_session().as_deref() == Some(view.session_id());
            self.clear_preview_mount_pending();
            if !still_valid {
                self.close_embedded_structured();
            }
            return !still_valid;
        }
        let desired = self.home.selected_structured_session();
        let mounted = self
            .home
            .structured_preview
            .as_ref()
            .map(|v| v.session_id());
        if desired.as_deref() == mounted {
            self.clear_preview_mount_pending();
            return false;
        }
        // Selection moved off the previewed session: drop the old preview
        // right away so the pane doesn't show a stale transcript.
        let Some(sid) = desired else {
            self.clear_preview_mount_pending();
            return self.home.structured_preview.take().is_some();
        };
        // Cheap discovery only: a down daemon keeps the "press Enter"
        // placeholder, and a stale mount is dropped.
        let Ok(endpoint) = crate::acp::client::discover() else {
            self.clear_preview_mount_pending();
            return self.home.structured_preview.take().is_some();
        };
        // A mount is coming: the renderer shows a quiet beat instead of
        // the wordy placeholder while we debounce + connect.
        self.home.structured_preview_pending = true;
        // Debounce: wait for the cursor to settle on this row so rapid
        // list navigation doesn't open a WebSocket per keystroke.
        const PREVIEW_MOUNT_DEBOUNCE: Duration = Duration::from_millis(120);
        let now = std::time::Instant::now();
        match &self.preview_mount_pending {
            Some((pending, _)) if *pending == sid => {}
            _ => {
                self.preview_mount_pending = Some((sid, now));
                return self.home.structured_preview.take().is_some();
            }
        }
        let settled = self
            .preview_mount_pending
            .as_ref()
            .is_some_and(|(_, at)| now.duration_since(*at) >= PREVIEW_MOUNT_DEBOUNCE);
        if !settled {
            return false;
        }
        let sid = self.preview_mount_pending.take().unwrap().0;
        self.connect_embedded_structured(endpoint, &sid).await;
        self.home.structured_preview_pending = false;
        true
    }

    fn execute_action(
        &mut self,
        action: Action,
        terminal: &mut Terminal<TuiBackend>,
    ) -> Result<()> {
        match action {
            Action::Quit => self.should_quit = true,
            Action::AttachSession(id) => {
                self.attach_session(&id, terminal)?;
            }
            Action::AttachTerminal(id, mode) => {
                self.attach_terminal(&id, mode);
            }
            Action::EditFile(path) => {
                self.edit_file(&path, terminal)?;
            }
            Action::StopSession(id) => {
                if self.home.get_instance(&id).is_none() {
                    return Ok(());
                }
                if let Err(error) = self.home.submit_daemon_stop_via_ui(&id) {
                    self.home.info_dialog = Some(crate::tui::dialogs::InfoDialog::new(
                        "Stop failed",
                        &error.to_string(),
                    ));
                }
            }
            Action::SetTheme(name) => {
                self.set_theme(&name);
            }
            Action::SpawnUpdate(method, version) => {
                if self.update_status_rx.is_some() {
                    self.set_status("update already in progress");
                    return Ok(());
                }
                self.spawn_update(method, version, terminal)?;
            }
            Action::SetTransientStatus(text) => {
                self.set_status(text);
            }
            Action::SpawnImagePull(image) => {
                if self.image_pull_rx.is_some() {
                    self.set_status("image pull already in progress");
                    return Ok(());
                }
                self.spawn_image_pull(image);
            }
            Action::SendMessage(id, message) => {
                // Cold starts show "Reviving" feedback; warm sessions skip the
                // toast, whose row would shift the preview for a frame.
                let warm = self.home.send_entry_is_warm(&id);
                if !warm {
                    self.home
                        .set_instance_status(&id, crate::session::Status::Starting);
                    self.set_status("Reviving session...");
                    self.draw(terminal)?;
                }
                let target = self.home.take_send_target();
                // Boot the pane at the size it will be shown at, so entering
                // live send does not immediately reflow it.
                let boot_size = self.home.live_send_boot_size();
                if let Err(error) = self
                    .home
                    .prepare_send_target(&id, target, message, boot_size)
                {
                    self.update_status = None;
                    self.home.info_dialog = Some(crate::tui::dialogs::InfoDialog::new(
                        "Send Failed",
                        &error.to_string(),
                    ));
                }
            }
            Action::EnterLiveSend(id) => {
                // Same revive flow as SendMessage.
                let warm = self.home.live_entry_is_warm(&id);
                if !warm {
                    self.home
                        .set_instance_status(&id, crate::session::Status::Starting);
                    self.set_status("Reviving session...");
                    self.draw(terminal)?;
                }
                let target = self.home.take_live_send_target();
                let boot_size = self.home.live_send_boot_size();
                if let Err(error) = self.home.prepare_live_send_target(&id, target, boot_size) {
                    // The receipt carries the geometry the user will see, so the
                    // entry itself happens there; a refusal here only needs to
                    // drop the toast and explain.
                    self.update_status = None;
                    self.home.info_dialog = Some(crate::tui::dialogs::InfoDialog::new(
                        "Live send failed",
                        &error.to_string(),
                    ));
                }
            }
            Action::AttachToolSession(id, tool_name) => {
                self.attach_tool_session(&id, &tool_name);
            }
            Action::RunBackgroundToolSession(id, tool_name) => {
                self.run_background_tool_session(&id, &tool_name);
            }
            // These need the async loop, which drains them after this returns.
            Action::OpenStructuredView(id) => self.pending_structured_view_open = Some(id),
            Action::SwitchSessionView(id) => self.pending_view_switch = Some(id),
            Action::StartDaemonThenOpenStructured(id) => self.pending_daemon_start_open = Some(id),
            Action::SmartRenameNow(id) => self.pending_smart_rename = Some(id),
        }
        Ok(())
    }

    /// Route a freshly-created session through the configured new-session
    /// mode. Shared by both creation paths (synchronous
    /// `apply_creation_results` handler) so the configured mode applies to
    /// every daemon-created session.
    ///
    /// A structured session skips the tmux modes entirely and opens its
    /// structured view (#2926): the wizard's Structured toggle is an
    /// explicit "drive this session in the structured view" ask, and
    /// both callers drain `pending_structured_view_open` right after
    /// this returns. Missing-instance race conditions fall through to
    /// `attach_session`: better the tmux-attach fallback than silently
    /// swallowing the new session.
    fn dispatch_new_session_attach(
        &mut self,
        session_id: &str,
        terminal: &mut Terminal<TuiBackend>,
    ) -> Result<()> {
        if self
            .home
            .get_instance(session_id)
            .is_some_and(|inst| inst.is_structured())
        {
            self.pending_structured_view_open = Some(session_id.to_string());
            return Ok(());
        }
        let mode = self.home.new_session_attach_mode(session_id);
        tracing::debug!(target: "tui.input",
            session_id = %session_id,
            mode = ?mode,
            "new session created; dispatching attach mode"
        );
        match mode {
            Some(crate::session::AttachMode::LiveSend) => {
                self.execute_action(Action::EnterLiveSend(session_id.to_string()), terminal)
            }
            Some(crate::session::AttachMode::Tmux) | None => {
                self.attach_session(session_id, terminal)
            }
        }
    }

    fn attach_session(
        &mut self,
        session_id: &str,
        terminal: &mut Terminal<TuiBackend>,
    ) -> Result<()> {
        let instance = match self.home.get_instance(session_id) {
            Some(inst) => inst.clone(),
            None => return Ok(()),
        };

        // Structured sessions have no tmux pane.
        if instance.is_structured() {
            return Ok(());
        }

        let tmux_session = instance.tmux_session()?;

        // Hook status and wrapper commands are stronger than shell detection
        // when deciding whether a running row needs an explicit restart.
        let exists = tmux_session.exists();
        let pane_dead = exists && tmux_session.is_pane_dead();
        let needs_restart = if !exists || pane_dead {
            true
        } else if crate::hooks::read_hook_status(&instance.id).is_some()
            || instance.has_command_override()
        {
            false
        } else {
            !instance.expects_shell() && tmux_session.is_pane_running_shell()
        };
        tracing::debug!(target: "tui.input",
            session_id,
            exists,
            pane_dead,
            needs_restart,
            "attach_session: restart decision"
        );
        // A row about to be started must not be one the user shelved: the
        // trashed/archived agent stays stopped, so refuse visibly and point at
        // the restore key instead of letting the runtime reject the start
        // (#4118). The predicate is this branch's own, so the Stopped case the
        // thin-client rework added to it is refused the same way.
        let starts_agent = needs_restart || instance.status == crate::session::Status::Stopped;
        if starts_agent && self.home.refuse_start_if_shelved(session_id) {
            return Ok(());
        }
        if starts_agent {
            // Show warning (once) if custom instruction is configured for an unsupported agent
            if instance.is_sandboxed() {
                let has_instruction = instance
                    .sandbox_info
                    .as_ref()
                    .and_then(|s| s.custom_instruction.as_ref())
                    .is_some_and(|i| !i.is_empty());

                if has_instruction
                    && crate::agents::get_agent(&instance.tool)
                        .is_none_or(|a| a.instruction_flag.is_none())
                {
                    let config = Config::load_or_warn();
                    if !config.app_state.has_seen_custom_instruction_warning {
                        self.home.info_dialog = Some(
                            crate::tui::dialogs::InfoDialog::new(
                                "Custom Instruction Not Supported",
                                &format!(
                                    "'{}' does not support custom instruction injection. The session will launch without the custom instruction.",
                                    instance.tool
                                ),
                            ),
                        );
                        self.home.pending_attach_after_warning = Some(session_id.to_string());

                        // A failed write only means the warning may show again.
                        if let Err(e) = update_app_state(|state| {
                            state.has_seen_custom_instruction_warning = true;
                        }) {
                            tracing::warn!(
                                target: "tui.input",
                                error = %e,
                                "failed to persist has_seen_custom_instruction_warning"
                            );
                        }

                        return Ok(());
                    }
                }
            }

            if instance.is_sandboxed()
                && self
                    .defer_to_store_move(session_id, Action::AttachSession(session_id.to_string()))
            {
                return Ok(());
            }

            let size = crate::terminal::get_size();
            if instance.status == crate::session::Status::Stopped {
                self.home.start_then_attach(session_id, size);
            } else {
                self.home.restart_then_attach(session_id, size, false);
            }
            return Ok(());
        }

        self.attach_live_session(session_id, terminal)
    }

    /// Attach to `session_id`'s running tmux pane and settle the row on return.
    fn attach_live_session(
        &mut self,
        session_id: &str,
        terminal: &mut Terminal<TuiBackend>,
    ) -> Result<()> {
        if !self.home.session_feed.native_interaction_available() {
            return Ok(());
        }
        let tmux_session = match self.home.get_instance(session_id) {
            Some(inst) => inst.tmux_session()?,
            None => return Ok(()),
        };
        // Undo manual preview sizing so the attaching client gets the full
        // terminal, and re-assert preview geometry afterwards.
        tmux_session.reset_size_to_latest_client();
        self.home.clear_preview_pane_sync(session_id);
        let attach_result = self.with_raw_mode_disabled(terminal, || tmux_session.attach())?;

        self.needs_redraw = true;
        crate::tmux::refresh_session_cache();
        self.home.reload()?;
        self.home.apply_session_feed();
        self.home.clear_unread_on_view(session_id);
        self.home.stamp_last_accessed(session_id);
        // In Attention sort, jump cursor to the top-attention row instead of
        // pinning it to the session we just came from; that session has
        // typically been bumped down a tier (Waiting → Running) and the next
        // item needing attention is now at row 0.
        self.select_after_attach(session_id);

        if let Err(e) = attach_result {
            tracing::warn!(target: "tui.input", "tmux attach returned error: {}", e);
        }

        Ok(())
    }

    /// If launching `session_id` would first copy its sandbox store, start
    /// that copy on the worker and come back to `resume` once it is done,
    /// returning `true`. The copy can take minutes; the status line narrates
    /// it meanwhile.
    fn defer_to_store_move(&mut self, session_id: &str, resume: Action) -> bool {
        if !self.home.needs_store_move_before_launch(session_id) {
            return false;
        }
        if !self.home.begin_store_move(session_id, Some(resume)) {
            self.set_status("another agent store move is still in progress");
        }
        true
    }

    fn attach_terminal(&mut self, session_id: &str, mode: TerminalMode) {
        let sandboxed = self
            .home
            .get_instance(session_id)
            .is_some_and(|row| row.is_sandboxed());
        let target = if mode == TerminalMode::Container && sandboxed {
            crate::session::AuxiliaryTarget::Container { index: 0 }
        } else {
            crate::session::AuxiliaryTarget::Host { index: 0 }
        };
        self.prepare_native_attachment(session_id, target);
    }

    fn attach_tool_session(&mut self, session_id: &str, tool_name: &str) {
        self.prepare_native_attachment(
            session_id,
            crate::session::AuxiliaryTarget::Tool {
                tool_name: tool_name.into(),
            },
        );
    }

    fn prepare_native_attachment(&mut self, id: &str, target: crate::session::AuxiliaryTarget) {
        use crate::tui::home::panes::{NativePane, PaneIntent};
        if let Err(error) = self.home.prepare_native_attachment(
            id,
            NativePane::Auxiliary(target),
            crate::terminal::get_size(),
            PaneIntent::Attach,
        ) {
            self.home.info_dialog = Some(crate::tui::dialogs::InfoDialog::new(
                "Attachment failed",
                &error.to_string(),
            ));
        }
    }

    fn attach_native_target(
        &mut self,
        ready: crate::tui::home::ReadyNativeAttachment,
        terminal: &mut Terminal<TuiBackend>,
    ) -> Result<()> {
        if !ready.lease.is_valid() {
            return Ok(());
        }
        let target = crate::tmux::Session::from_name(&ready.tmux_name);
        let lease = ready.lease;
        let attach_result = self.with_raw_mode_disabled(
            terminal,
            Box::new(move || {
                anyhow::ensure!(lease.is_valid(), "Native interaction permission revoked");
                target.attach()
            }),
        )?;
        self.needs_redraw = true;
        self.home.apply_session_feed();
        self.select_after_attach(&ready.id);
        if let Err(error) = attach_result {
            tracing::warn!(target: "tui.input", %error, "Native tmux attach failed");
            self.set_status(format!("Attach failed: {error}"));
        }
        Ok(())
    }

    fn select_after_attach(&mut self, session_id: &str) {
        if self.home.sort_order() == crate::session::config::SortOrder::Attention {
            self.home.select_top_attention(Some(session_id));
        } else {
            self.home.select_session_by_id(session_id);
        }
    }

    fn run_background_tool_session(&mut self, session_id: &str, tool_name: &str) {
        let Some(project_path) = self
            .home
            .get_instance(session_id)
            .map(|i| i.project_path.as_str())
        else {
            self.set_status(format!("Tool '{tool_name}' failed: session not found"));
            return;
        };
        let Some(command) = self
            .home
            .tool_configs
            .get(tool_name)
            .map(|t| t.command.as_str())
        else {
            self.set_status(format!("Tool '{tool_name}' is not configured"));
            return;
        };
        let status = match spawn_background_tool(session_id, tool_name, project_path, command) {
            Ok(()) => format!("Started background tool: {tool_name}"),
            Err(e) => format!("Failed to start background tool '{tool_name}': {e}"),
        };
        self.set_status(status);
    }

    fn edit_file(
        &mut self,
        path: &std::path::Path,
        terminal: &mut Terminal<TuiBackend>,
    ) -> Result<()> {
        // Determine which editor to use (prefer vim, fall back to nano)
        let editor = std::env::var("EDITOR")
            .ok()
            .or_else(|| {
                ["vim", "nano"]
                    .into_iter()
                    .find(|name| {
                        std::process::Command::new(name)
                            .arg("--version")
                            .stdout(std::process::Stdio::null())
                            .stderr(std::process::Stdio::null())
                            .status()
                            .is_ok()
                    })
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| "vim".to_string());

        let path = path.to_owned();
        let editor_clone = editor.clone();
        let status = self.with_raw_mode_disabled(terminal, move || {
            let mut cmd = std::process::Command::new(&editor_clone);
            cmd.arg(&path);
            // SIG_IGN from `IgnoreSignalsGuard` would survive exec.
            #[cfg(unix)]
            crate::process::reset_signals_on_exec(&mut cmd);
            cmd.status()
        })?;

        self.needs_redraw = true;

        // Refresh diff view if it's open (file may have changed)
        if let Some(ref mut diff_view) = self.home.diff_view {
            if let Err(e) = diff_view.refresh_files() {
                tracing::warn!(target: "tui.input", "Failed to refresh diff after edit: {}", e);
            }
        }

        // Log any editor errors but don't fail
        if let Err(e) = status {
            tracing::warn!(target: "tui.input", "Editor '{}' returned error: {}", editor, e);
        }

        Ok(())
    }
}

fn spawn_background_tool(
    session_id: &str,
    tool_name: &str,
    working_dir: &str,
    command: &str,
) -> Result<()> {
    if command.trim().is_empty() {
        anyhow::bail!("Tool '{}' has no command configured", tool_name);
    }

    let shell = crate::session::environment::user_shell();
    let mut child_command = std::process::Command::new(&shell);
    child_command
        .arg("-c")
        .arg(command)
        .current_dir(working_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        child_command.process_group(0);
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;

        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

        child_command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }

    let child = child_command.spawn().with_context(|| {
        format!(
            "spawn background tool '{}' with shell '{}'",
            tool_name, shell
        )
    })?;
    wait_for_background_tool(session_id, tool_name, child);
    Ok(())
}

fn wait_for_background_tool(session_id: &str, tool_name: &str, mut child: std::process::Child) {
    let session_id = session_id.to_string();
    let tool_name = tool_name.to_string();
    std::thread::spawn(move || match child.wait() {
        Ok(status) if status.success() => {
            tracing::debug!(
                target: "tui.tools",
                session_id = %session_id,
                tool = %tool_name,
                status = %status,
                "background tool exited"
            );
        }
        Ok(status) => {
            tracing::warn!(
                target: "tui.tools",
                session_id = %session_id,
                tool = %tool_name,
                status = %status,
                "background tool exited unsuccessfully"
            );
        }
        Err(e) => {
            tracing::warn!(
                target: "tui.tools",
                session_id = %session_id,
                tool = %tool_name,
                error = %e,
                "failed waiting for background tool"
            );
        }
    });
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Quit,
    AttachSession(String),
    AttachTerminal(String, TerminalMode),
    EditFile(PathBuf),
    StopSession(String),
    SetTheme(String),
    SpawnUpdate(crate::update::install::InstallMethod, String),
    SetTransientStatus(String),
    /// Pull the sandbox image after the user accepts the "image update
    /// available" banner's confirm. Deferred to `execute_action` so the loop
    /// can show a "pulling…" status before the blocking pull starts.
    SpawnImagePull(String),
    /// Send a message to a session. Deferred to `execute_action` (rather
    /// than handled inline in the dialog Submit branch) so the app loop
    /// can render a "Reviving..." status before the potentially-slow
    /// ensure_pane_ready call.
    SendMessage(String, String),
    /// Enter live-send mode on a session. Same revive-and-stage pattern
    /// as `SendMessage`: the deferred action lets the app loop render the
    /// "Reviving..." toast before `ensure_pane_ready` runs, then the home
    /// view flips into the live-send capture state for subsequent keys.
    EnterLiveSend(String),
    /// Open a session that was just created via the synchronous create path
    /// (no sandbox, hooks, or worktree). This action routes through the
    /// new-session mode, like the async path in `apply_creation_results`.
    /// `AttachSession` is already resolved for an existing session row.
    /// Attach to a tool session (lazygit, yazi, etc.) for the given agent
    /// session. The tool_name indexes into Config.tools.
    AttachToolSession(String, String),
    /// Run a configured tool command without creating or attaching a tmux tool
    /// session. The command runs in the selected agent session's workdir.
    RunBackgroundToolSession(String, String),
    /// Open the native acp view for `session_id`. The action handler
    /// stashes the id in `pending_structured_view_open`; the main loop drains it
    /// after `execute_action` returns and runs the async acp loop
    /// against the borrowed terminal + event stream.
    OpenStructuredView(String),
    /// Flip a session's persisted view (structured ↔ terminal) through the
    /// daemon's switch endpoints. Stashed in `pending_view_switch` (the
    /// POST needs the async loop) and drained alongside
    /// `pending_structured_view_open`; the daemon persists the change and
    /// the file watcher refreshes the row.
    SwitchSessionView(String),
    /// The Yes on the "no daemon running, start a local one?" confirm
    /// shown when opening a structured view. Stashed in
    /// `pending_daemon_start_open` (spawn + health wait must be
    /// awaited) and drained alongside the other structured stashes.
    StartDaemonThenOpenStructured(String),
    /// On-demand "Auto-name now" for a structured session. Stashed in
    /// `pending_smart_rename` (the daemon POST needs the async loop) and
    /// drained alongside the other structured stashes (#3039).
    SmartRenameNow(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::SendOutcome;
    use std::sync::atomic::Ordering;

    /// Query a signal's current disposition without leaving it changed:
    /// `sigaction` always both sets and returns the previous value, so we
    /// immediately set it back to what we just read.
    #[cfg(unix)]
    fn current_disposition(signal: nix::sys::signal::Signal) -> nix::sys::signal::SigHandler {
        use nix::sys::signal::{sigaction, SaFlags, SigAction, SigHandler, SigSet};

        let probe = SigAction::new(SigHandler::SigDfl, SaFlags::empty(), SigSet::empty());
        // SAFETY: SIG_DFL is async-signal-safe; this runs outside a signal handler.
        let prev = unsafe { sigaction(signal, &probe) }.expect("sigaction query");
        // SAFETY: restoring what was just read is likewise safe.
        unsafe { sigaction(signal, &prev) }.expect("sigaction restore");
        prev.handler()
    }

    #[cfg(unix)]
    fn same_disposition(a: nix::sys::signal::SigHandler, b: nix::sys::signal::SigHandler) -> bool {
        use nix::sys::signal::SigHandler;
        match (a, b) {
            (SigHandler::SigDfl, SigHandler::SigDfl) => true,
            (SigHandler::SigIgn, SigHandler::SigIgn) => true,
            (SigHandler::Handler(f1), SigHandler::Handler(f2)) => f1 as usize == f2 as usize,
            _ => false,
        }
    }

    /// A failed replacement must not consume the previous view's draft.
    #[tokio::test]
    #[serial_test::serial]
    async fn drain_paste_forwards_to_the_mounted_view_and_keeps_other_targets() {
        let temp = tempfile::TempDir::new().unwrap();
        let _guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        crate::session::get_profile_dir("test").unwrap();
        let mut app = App::new(
            "test",
            crate::tmux::AvailableTools::with_tools(&["claude"]),
            true,
            false,
            crate::file_watch::FileWatchService::noop(),
        )
        .expect("app");
        app.home
            .pending_paste_for_structured_view
            .insert("s-1".into(), "buffered draft".into());
        app.home
            .pending_paste_for_structured_view
            .insert("s-2".into(), "other target".into());
        app.home.structured_preview =
            Some(crate::tui::structured_view::embedded::EmbeddedView::for_test("s-1"));

        // A failed attempt to mount s-2 can leave s-1 mounted.
        app.drain_pending_paste_for_structured_view("s-2").await;
        assert_eq!(
            app.home
                .structured_preview
                .as_ref()
                .unwrap()
                .composer_text(),
            ""
        );
        assert_eq!(
            app.home
                .pending_paste_for_structured_view
                .get("s-1")
                .map(String::as_str),
            Some("buffered draft"),
        );

        app.drain_pending_paste_for_structured_view("s-1").await;
        app.drain_pending_paste_for_structured_view("s-1").await;

        let preview = app.home.structured_preview.as_ref().expect("mounted");
        assert_eq!(
            preview.composer_text(),
            "buffered draft",
            "the captured buffer must land in the mounted composer"
        );
        assert_eq!(
            app.home.pending_paste_for_structured_view.get("s-1"),
            None,
            "a forwarded buffer must be consumed"
        );
        assert_eq!(
            app.home.pending_paste_for_structured_view.get("s-2"),
            Some(&"other target".to_string()),
            "another target's unsent draft must survive the drain"
        );
    }

    // Signal disposition is process-global state, so this must not run
    // concurrently with any other test that installs a handler for
    // SIGINT/SIGQUIT.
    #[test]
    #[cfg(unix)]
    #[serial_test::serial]
    fn ignore_signals_guard_installs_sig_ign_and_restores_prior_disposition() {
        use nix::sys::signal::{SigHandler, Signal};

        let baseline_sigint = current_disposition(Signal::SIGINT);
        let baseline_sigquit = current_disposition(Signal::SIGQUIT);

        let guard = IgnoreSignalsGuard::new();
        assert!(
            same_disposition(current_disposition(Signal::SIGINT), SigHandler::SigIgn),
            "SIGINT should be ignored while the guard is alive"
        );
        assert!(
            same_disposition(current_disposition(Signal::SIGQUIT), SigHandler::SigIgn),
            "SIGQUIT should be ignored while the guard is alive"
        );

        drop(guard);

        assert!(
            same_disposition(current_disposition(Signal::SIGINT), baseline_sigint),
            "SIGINT disposition should be restored after the guard drops"
        );
        assert!(
            same_disposition(current_disposition(Signal::SIGQUIT), baseline_sigquit),
            "SIGQUIT disposition should be restored after the guard drops"
        );
    }

    /// The theme idempotency guard must treat both the name AND the palette
    /// mode as part of the theme identity, and report "no change" only when
    /// both match. This is what keeps a config-file-watcher theme re-dispatch
    /// (fired on every `config.toml` save: sidebar-collapse persistence, list
    /// resize, `i`, settings) from forcing a flickering full-screen clear.
    #[test]
    fn theme_apply_needed_compares_name_and_palette_mode() {
        assert!(
            !theme_apply_needed(("empire", false), ("empire", false)),
            "identical name + mode is a no-op (no redraw, no clear)"
        );
        assert!(
            theme_apply_needed(("empire", false), ("zinc", false)),
            "a different name must re-apply"
        );
        assert!(
            theme_apply_needed(("empire", false), ("empire", true)),
            "a different palette mode must re-apply even with the same name"
        );
    }

    /// The pre-draw `cursor::Hide` is skipped only in the one state that
    /// visibly flickers on non-synchronized-update terminals: live-send
    /// active with no overlay on top of it. Any overlay (IME-relevant local
    /// input) or a non-live-send state keeps the Hide.
    #[test]
    fn skip_predraw_cursor_hide_only_in_live_send_without_overlay() {
        assert!(
            skip_predraw_cursor_hide(true, false),
            "live-send with no overlay must skip the pre-draw Hide (the fix)"
        );
        assert!(
            !skip_predraw_cursor_hide(true, true),
            "live-send with an overlay open must keep the Hide for IME protection"
        );
        assert!(
            !skip_predraw_cursor_hide(false, false),
            "outside live-send the Hide must stay unchanged"
        );
        assert!(
            !skip_predraw_cursor_hide(false, true),
            "outside live-send with an overlay the Hide must stay unchanged"
        );
    }

    // The TUI create counter is a process-global static, so these tests mutate
    // shared state. `#[serial]` (with the `telemetry_creates` group key) keeps
    // them from racing each other; each resets the counter to a known base
    // first rather than assuming a clean start.
    fn reset_creates(to: u32) {
        TUI_SESSION_CREATES.store(to, Ordering::Relaxed);
    }

    // #1897: a confirmed send clears only what the snapshot reported, so a create
    // that lands between the snapshot build and the confirmed send survives into
    // the next snapshot instead of being reset away. Mirrors the serve-side
    // `reported_count_decrement_preserves_concurrent_increments`.
    #[test]
    #[serial_test::serial(telemetry_creates)]
    fn create_counter_clear_preserves_in_flight_create() {
        reset_creates(0);
        record_session_create();
        record_session_create();
        record_session_create();
        // The snapshot reported the 3 creates seen at build time.
        let reported = reported_session_creates();
        assert_eq!(reported, 3);
        // One more create lands while the snapshot is in flight.
        record_session_create();
        clear_reported_session_creates(reported, SendOutcome::Sent);
        assert_eq!(
            TUI_SESSION_CREATES.load(Ordering::Relaxed),
            1,
            "the create that arrived during the send must be retained"
        );
    }

    // A failed or deduped send must retain the full count so the next snapshot
    // re-reports it; only a confirmed `Sent` consumes the reported value.
    #[test]
    #[serial_test::serial(telemetry_creates)]
    fn create_counter_clear_retains_on_unconfirmed_send() {
        for outcome in [SendOutcome::Failed, SendOutcome::Deduped] {
            reset_creates(0);
            record_session_create();
            record_session_create();
            let reported = reported_session_creates();
            clear_reported_session_creates(reported, outcome);
            assert_eq!(
                TUI_SESSION_CREATES.load(Ordering::Relaxed),
                2,
                "{outcome:?} must retain the count for the next snapshot"
            );
        }
    }

    // A zero report is a no-op, and the decrement saturates rather than
    // underflow-wrapping the AtomicU32 (cheap insurance against a future
    // double-clear), mirroring the serve saturation test.
    #[test]
    #[serial_test::serial(telemetry_creates)]
    fn create_counter_clear_is_noop_for_zero_and_saturates() {
        reset_creates(3);
        clear_reported_session_creates(0, SendOutcome::Sent);
        assert_eq!(TUI_SESSION_CREATES.load(Ordering::Relaxed), 3);

        reset_creates(2);
        clear_reported_session_creates(5, SendOutcome::Sent);
        assert_eq!(TUI_SESSION_CREATES.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn image_banner_shows_only_when_it_owns_the_row() {
        // The plain case: an image update is pending and nothing outranks it.
        assert!(should_show_image_banner(true, false, false, false));
        // No pending update: nothing to show.
        assert!(!should_show_image_banner(false, false, false, false));
        // The app-update banner and any transient toast both outrank it.
        assert!(!should_show_image_banner(true, true, false, false));
        assert!(!should_show_image_banner(true, false, true, false));
    }

    #[test]
    fn image_banner_stays_hidden_while_its_own_pull_runs() {
        // #2072: once the user accepts the pull, the banner must stay down for
        // the whole `docker pull`. Even after the "pulling…" toast clears
        // (has_status = false) the in-flight pull keeps the banner hidden, so it
        // can't redraw under the pull and re-arm `u` into "pull already in
        // progress".
        assert!(!should_show_image_banner(true, false, false, true));
        assert!(!should_show_image_banner(true, false, true, true));
    }

    #[test]
    fn ctrl_q_never_quits() {
        // The whole point of #1569: Ctrl+Q is a live-mode-exit habit and
        // must not close aoe from the home view, regardless of the other
        // flags.
        for creation_pending in [false, true] {
            for confirm in [false, true] {
                assert_eq!(
                    quit_intent(KeyModifiers::CONTROL, creation_pending, confirm),
                    QuitIntent::Ignore,
                );
            }
        }
    }

    #[test]
    fn plain_q_quits_when_confirm_disabled() {
        assert_eq!(
            quit_intent(KeyModifiers::NONE, false, false),
            QuitIntent::Quit,
        );
    }

    #[test]
    fn plain_q_confirms_when_enabled() {
        assert_eq!(
            quit_intent(KeyModifiers::NONE, false, true),
            QuitIntent::Confirm,
        );
    }

    #[test]
    fn creation_pending_confirms_before_anything_else() {
        // Creation-in-progress takes precedence over the quit confirm so
        // the user is warned the hook will be cancelled.
        assert_eq!(
            quit_intent(KeyModifiers::NONE, true, true),
            QuitIntent::ConfirmDuringCreation,
        );
        assert_eq!(
            quit_intent(KeyModifiers::NONE, true, false),
            QuitIntent::ConfirmDuringCreation,
        );
    }

    #[test]
    fn heartbeat_wins_when_both_disk_paths_are_ready() {
        assert_eq!(
            decide_disk_refresh(true, true, true),
            DiskRefreshDecision::FullHeartbeat,
            "when live-idle and both heartbeat and watcher are ready, the full reload wins"
        );
        assert_eq!(
            decide_disk_refresh(true, true, false),
            DiskRefreshDecision::FullHeartbeat,
            "heartbeat fires even without a watcher kick"
        );
        assert_eq!(
            decide_disk_refresh(true, false, true),
            DiskRefreshDecision::StorageOnly,
            "watcher kick alone fires the storage-only path"
        );
        assert_eq!(
            decide_disk_refresh(true, false, false),
            DiskRefreshDecision::None,
            "no inputs ready yields no refresh"
        );
    }

    #[test]
    fn live_send_uses_storage_only_for_watcher_and_heartbeat() {
        assert_eq!(
            decide_disk_refresh(false, false, false),
            DiskRefreshDecision::None,
            "live-send with no refresh input must remain idle"
        );
        assert_eq!(
            decide_disk_refresh(false, true, false),
            DiskRefreshDecision::StorageOnly,
            "live-send must use a storage-only heartbeat fallback"
        );
        for &heartbeat in &[false, true] {
            assert_eq!(
                decide_disk_refresh(false, heartbeat, true),
                DiskRefreshDecision::StorageOnly,
                "live-send must allow the storage-only watcher path (heartbeat={heartbeat})"
            );
        }
    }

    #[test]
    fn full_heartbeat_deferred_during_live_send_runs_on_exit() {
        let live_plan = plan_disk_refresh(false, true, false, false);
        assert_eq!(live_plan.decision, DiskRefreshDecision::StorageOnly);
        assert!(live_plan.full_heartbeat_deferred);

        let idle_plan = plan_disk_refresh(true, false, false, live_plan.full_heartbeat_deferred);
        assert_eq!(idle_plan.decision, DiskRefreshDecision::FullHeartbeat);
        assert!(!idle_plan.full_heartbeat_deferred);
    }

    #[test]
    fn caller_consumes_dirty_latch_during_live_send() {
        let dirty_atomic = std::sync::atomic::AtomicBool::new(true);
        let dirty = dirty_atomic.swap(false, std::sync::atomic::Ordering::Acquire);

        assert!(dirty, "live-send must consume the watcher kick");
        assert!(
            !dirty_atomic.load(std::sync::atomic::Ordering::Acquire),
            "a consumed watcher kick must not remain latched"
        );
        assert_eq!(
            decide_disk_refresh(false, true, dirty),
            DiskRefreshDecision::StorageOnly,
            "the consumed kick must choose storage-only refresh while the full heartbeat remains deferred"
        );
    }

    #[test]
    fn config_refresh_kick_is_gated_by_live_send() {
        let dirty = std::sync::atomic::AtomicBool::new(true);
        assert!(
            !take_config_refresh_kick(false, &dirty),
            "live-send must defer config refreshes"
        );
        assert!(
            dirty.load(std::sync::atomic::Ordering::Acquire),
            "live-send must leave config_dirty latched for the next eligible tick"
        );
    }

    #[test]
    fn config_refresh_and_disk_refresh_can_coexist_in_one_tick() {
        let config_dirty = std::sync::atomic::AtomicBool::new(true);
        let disk_dirty = std::sync::atomic::AtomicBool::new(true);

        let config_kick = take_config_refresh_kick(true, &config_dirty);
        // Mirrors the tick-loop gating: live_idle is true here, so the
        // caller swaps the latch and passes the consumed value to the
        // pure helper.
        let dirty = disk_dirty.swap(false, std::sync::atomic::Ordering::Acquire);
        let disk_decision = decide_disk_refresh(true, true, dirty);

        assert!(config_kick, "config refresh must be scheduled first");
        assert_eq!(disk_decision, DiskRefreshDecision::FullHeartbeat);
        assert!(!config_dirty.load(std::sync::atomic::Ordering::Acquire));
        assert!(!disk_dirty.load(std::sync::atomic::Ordering::Acquire));
    }

    #[test]
    fn test_action_enum() {
        let quit = Action::Quit;
        let attach = Action::AttachSession("test-id".to_string());
        let attach_terminal =
            Action::AttachTerminal("test-id".to_string(), TerminalMode::Container);

        assert_eq!(quit, Action::Quit);
        assert_eq!(attach, Action::AttachSession("test-id".to_string()));
        assert_eq!(
            attach_terminal,
            Action::AttachTerminal("test-id".to_string(), TerminalMode::Container)
        );
    }

    #[test]
    fn test_action_clone() {
        let original = Action::AttachSession("session-123".to_string());
        let cloned = original.clone();
        assert_eq!(original, cloned);

        let terminal_action = Action::AttachTerminal("session-123".to_string(), TerminalMode::Host);
        let terminal_cloned = terminal_action.clone();
        assert_eq!(terminal_action, terminal_cloned);
    }

    #[test]
    fn test_poll_update_check_returns_true_when_update_available() {
        // Create a oneshot channel and send an update notification
        let (tx, rx) = tokio::sync::oneshot::channel();
        let update_info = UpdateInfo {
            available: true,
            current_version: "0.4.0".to_string(),
            latest_version: "0.5.0".to_string(),
        };
        tx.send(Ok(update_info)).unwrap();

        // poll_update_receiver should return true when an update is available
        let (info, rx_out, received) = poll_update_receiver(Some(rx), None);
        assert!(received);
        assert!(info.is_some());
        assert_eq!(info.as_ref().unwrap().latest_version, "0.5.0");
        assert!(rx_out.is_none()); // Channel consumed
    }

    #[test]
    fn test_poll_update_check_returns_false_when_no_update() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let update_info = UpdateInfo {
            available: false,
            current_version: "0.5.0".to_string(),
            latest_version: "0.5.0".to_string(),
        };
        tx.send(Ok(update_info)).unwrap();

        // poll_update_receiver should return false when no update available
        let (info, rx_out, received) = poll_update_receiver(Some(rx), None);
        assert!(!received);
        assert!(info.is_none());
        assert!(rx_out.is_none()); // Channel consumed even though no update
    }

    #[test]
    fn test_poll_update_check_returns_false_when_channel_empty() {
        let (_tx, rx) = tokio::sync::oneshot::channel::<anyhow::Result<UpdateInfo>>();

        // poll_update_receiver should return false when channel is empty
        let (info, rx_out, received) = poll_update_receiver(Some(rx), None);
        assert!(!received);
        assert!(info.is_none());
        // Receiver should be put back for next poll
        assert!(rx_out.is_some());
    }

    #[test]
    fn periodic_recheck_fires_after_interval_elapses() {
        // The dominant bug (#1471): the original code spawned the update check
        // only once at startup. After the configured interval has passed in a
        // long-running TUI, the loop must spawn a fresh check.
        let interval = Duration::from_secs(24 * 3600);
        assert!(should_spawn_periodic_update_check(
            Some(interval + Duration::from_secs(1)),
            interval,
            false,
            true,
        ));
    }

    #[test]
    fn periodic_recheck_holds_within_interval() {
        let interval = Duration::from_secs(24 * 3600);
        assert!(!should_spawn_periodic_update_check(
            Some(interval - Duration::from_secs(1)),
            interval,
            false,
            true,
        ));
    }

    #[test]
    fn periodic_recheck_skips_when_in_flight() {
        // Don't queue a second check while one is already running; the existing
        // one will deliver its result on the oneshot channel and the next tick
        // after that can fire normally.
        let interval = Duration::from_secs(24 * 3600);
        assert!(!should_spawn_periodic_update_check(
            Some(interval + Duration::from_secs(1)),
            interval,
            true,
            true,
        ));
    }

    #[test]
    fn periodic_recheck_skips_when_mode_disabled() {
        // update_check_mode = "off" should suppress both startup and periodic
        // checks. Mirror the gate at startup.
        let interval = Duration::from_secs(24 * 3600);
        assert!(!should_spawn_periodic_update_check(
            Some(interval + Duration::from_secs(1)),
            interval,
            false,
            false,
        ));
    }

    #[test]
    fn periodic_recheck_fires_immediately_when_never_checked_and_mode_enabled() {
        // User started with mode=off, toggled to notify/auto mid-session. The
        // first guard tick after toggle should fire without waiting another
        // full `PERIODIC_RECHECK_INTERVAL` from process launch.
        let interval = Duration::from_secs(24 * 3600);
        assert!(should_spawn_periodic_update_check(
            None, interval, false, true,
        ));
    }

    #[test]
    fn periodic_recheck_skips_when_never_checked_but_mode_disabled() {
        // Symmetric: a None elapsed does not override the mode gate. Mode=off
        // still wins.
        let interval = Duration::from_secs(24 * 3600);
        assert!(!should_spawn_periodic_update_check(
            None, interval, false, false,
        ));
    }

    #[test]
    fn periodic_recheck_fires_at_interval_boundary() {
        // `>=`, not `>`. The tick fires at the interval mark, not
        // interval + epsilon.
        let interval = Duration::from_secs(3600);
        assert!(should_spawn_periodic_update_check(
            Some(interval),
            interval,
            false,
            true,
        ));
    }

    #[test]
    fn test_poll_update_check_preserves_existing_info() {
        // If we already have update info and the channel is closed, preserve the existing info
        let existing_info = UpdateInfo {
            available: true,
            current_version: "0.4.0".to_string(),
            latest_version: "0.5.0".to_string(),
        };

        // No receiver, just existing info
        let (info, rx_out, received) = poll_update_receiver(None, Some(existing_info));
        assert!(!received); // No new update received
        assert!(info.is_some()); // But existing info is preserved
        assert_eq!(info.as_ref().unwrap().latest_version, "0.5.0");
        assert!(rx_out.is_none());
    }

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn burst_candidate_accepts_printable_chars_and_enter() {
        assert!(App::is_burst_candidate(&key(
            KeyCode::Char('a'),
            KeyModifiers::NONE
        )));
        assert!(App::is_burst_candidate(&key(
            KeyCode::Char(' '),
            KeyModifiers::NONE
        )));
        assert!(App::is_burst_candidate(&key(
            KeyCode::Char('A'),
            KeyModifiers::SHIFT
        )));
        assert!(App::is_burst_candidate(&key(
            KeyCode::Enter,
            KeyModifiers::NONE
        )));
    }

    #[test]
    fn burst_candidate_rejects_modified_chords_and_nav_keys() {
        // Ctrl/Alt chords are intentional shortcuts, never paste burst chars.
        assert!(!App::is_burst_candidate(&key(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL
        )));
        assert!(!App::is_burst_candidate(&key(
            KeyCode::Char('b'),
            KeyModifiers::ALT
        )));
        // Navigation/control keys are not burst candidates.
        assert!(!App::is_burst_candidate(&key(
            KeyCode::Esc,
            KeyModifiers::NONE
        )));
        assert!(!App::is_burst_candidate(&key(
            KeyCode::Tab,
            KeyModifiers::NONE
        )));
        assert!(!App::is_burst_candidate(&key(
            KeyCode::Up,
            KeyModifiers::NONE
        )));
        assert!(!App::is_burst_candidate(&key(
            KeyCode::Backspace,
            KeyModifiers::NONE
        )));
    }

    #[test]
    fn auto_repeat_burst_rejects_held_navigation_but_not_pasted_text() {
        let cases = [
            (
                vec![
                    key(KeyCode::Char('j'), KeyModifiers::NONE),
                    key(KeyCode::Char('j'), KeyModifiers::NONE),
                    key(KeyCode::Char('j'), KeyModifiers::NONE),
                ],
                true,
            ),
            (
                vec![
                    key(KeyCode::Char('k'), KeyModifiers::NONE),
                    key(KeyCode::Char('k'), KeyModifiers::NONE),
                    key(KeyCode::Char('k'), KeyModifiers::NONE),
                ],
                true,
            ),
            (
                vec![
                    KeyEvent::new_with_kind(
                        KeyCode::Char('j'),
                        KeyModifiers::NONE,
                        KeyEventKind::Press,
                    ),
                    KeyEvent::new_with_kind(
                        KeyCode::Char('j'),
                        KeyModifiers::NONE,
                        KeyEventKind::Repeat,
                    ),
                    KeyEvent::new_with_kind(
                        KeyCode::Char('j'),
                        KeyModifiers::NONE,
                        KeyEventKind::Repeat,
                    ),
                ],
                true,
            ),
            (
                vec![
                    key(KeyCode::Char('p'), KeyModifiers::NONE),
                    key(KeyCode::Char('a'), KeyModifiers::NONE),
                    key(KeyCode::Char('s'), KeyModifiers::NONE),
                    key(KeyCode::Char('t'), KeyModifiers::NONE),
                    key(KeyCode::Char('e'), KeyModifiers::NONE),
                ],
                false,
            ),
        ];
        for (keys, expected) in cases {
            assert_eq!(App::is_auto_repeat_burst(&keys), expected, "{keys:?}");
        }
    }

    #[test]
    fn burst_char_for_matches_is_burst_candidate_domain() {
        // Contract: any key that passes is_burst_candidate must also yield
        // Some from burst_char_for, otherwise the event-loop's expect() panics.
        let candidates = [
            key(KeyCode::Char('a'), KeyModifiers::NONE),
            key(KeyCode::Char(' '), KeyModifiers::NONE),
            key(KeyCode::Char('A'), KeyModifiers::SHIFT),
            key(KeyCode::Char('~'), KeyModifiers::NONE),
            key(KeyCode::Enter, KeyModifiers::NONE),
        ];
        for k in &candidates {
            assert!(App::is_burst_candidate(k));
            assert!(
                App::burst_char_for(k).is_some(),
                "burst_char_for must agree with is_burst_candidate for {:?}",
                k
            );
        }
        assert_eq!(
            App::burst_char_for(&key(KeyCode::Enter, KeyModifiers::NONE)),
            Some('\n'),
            "Enter must map to \\n so embedded sentence-breaks land in the burst"
        );
    }

    #[test]
    fn split_trailing_enter_peels_terminating_enter() {
        // Regression: typing "hi<Enter>" with <5ms key gaps used to land
        // as a single burst whose string `"hi\n"` was forwarded to
        // handle_paste, so the textarea inserted `\n` as data and the
        // dialog's Submit branch never fired. Peel the trailing Enter
        // so handle_paste sees `"hi"` and we replay Enter for Submit.
        let burst_keys = vec![
            key(KeyCode::Char('h'), KeyModifiers::NONE),
            key(KeyCode::Char('i'), KeyModifiers::NONE),
            key(KeyCode::Enter, KeyModifiers::NONE),
        ];
        let (paste, enter) = App::split_trailing_enter("hi\n", &burst_keys);
        assert_eq!(paste, "hi");
        assert!(enter.is_some());
        assert_eq!(enter.unwrap().code, KeyCode::Enter);
    }

    #[test]
    fn split_trailing_enter_preserves_embedded_newlines() {
        // Voice/dictation pastes with sentence breaks land embedded
        // Enters in the burst. Those are data, not intent-to-submit.
        // Only the trailing Enter is peeled.
        let burst_keys = vec![
            key(KeyCode::Char('a'), KeyModifiers::NONE),
            key(KeyCode::Enter, KeyModifiers::NONE),
            key(KeyCode::Char('b'), KeyModifiers::NONE),
            key(KeyCode::Enter, KeyModifiers::NONE),
        ];
        let (paste, enter) = App::split_trailing_enter("a\nb\n", &burst_keys);
        assert_eq!(paste, "a\nb");
        assert!(enter.is_some());
    }

    #[test]
    fn split_trailing_enter_keeps_mid_burst_enter_when_burst_ends_on_char() {
        // Burst ends on a printable char, so there is no trailing Enter to peel.
        // The embedded Enter stays in the paste text.
        let burst_keys = vec![
            key(KeyCode::Char('h'), KeyModifiers::NONE),
            key(KeyCode::Char('i'), KeyModifiers::NONE),
            key(KeyCode::Enter, KeyModifiers::NONE),
            key(KeyCode::Char('t'), KeyModifiers::NONE),
            key(KeyCode::Char('h'), KeyModifiers::NONE),
            key(KeyCode::Char('e'), KeyModifiers::NONE),
            key(KeyCode::Char('r'), KeyModifiers::NONE),
            key(KeyCode::Char('e'), KeyModifiers::NONE),
        ];
        let (paste, enter) = App::split_trailing_enter("hi\nthere", &burst_keys);
        assert_eq!(paste, "hi\nthere");
        assert!(enter.is_none());
    }

    #[test]
    fn split_trailing_enter_no_enter_at_all() {
        let burst_keys = vec![
            key(KeyCode::Char('a'), KeyModifiers::NONE),
            key(KeyCode::Char('b'), KeyModifiers::NONE),
            key(KeyCode::Char('c'), KeyModifiers::NONE),
        ];
        let (paste, enter) = App::split_trailing_enter("abc", &burst_keys);
        assert_eq!(paste, "abc");
        assert!(enter.is_none());
    }

    #[test]
    fn split_trailing_enter_single_enter_yields_empty_paste() {
        // Pathological: burst is just an Enter. paste_text is empty;
        // caller skips handle_paste and only replays the Enter so
        // Submit fires on whatever is in the textarea.
        let burst_keys = vec![key(KeyCode::Enter, KeyModifiers::NONE)];
        let (paste, enter) = App::split_trailing_enter("\n", &burst_keys);
        assert_eq!(paste, "");
        assert!(enter.is_some());
    }

    #[test]
    fn split_trailing_enter_consecutive_trailing_enters_only_peels_last() {
        // Two trailing Enters: keep the first as data (the user's
        // intentional blank-line break) and peel only the last for
        // Submit.
        let burst_keys = vec![
            key(KeyCode::Char('h'), KeyModifiers::NONE),
            key(KeyCode::Char('i'), KeyModifiers::NONE),
            key(KeyCode::Enter, KeyModifiers::NONE),
            key(KeyCode::Enter, KeyModifiers::NONE),
        ];
        let (paste, enter) = App::split_trailing_enter("hi\n\n", &burst_keys);
        assert_eq!(paste, "hi\n");
        assert!(enter.is_some());
    }
}
