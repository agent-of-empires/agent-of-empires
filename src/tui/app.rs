//! Main TUI application

use anyhow::Result;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use futures_util::StreamExt;
use ratatui::prelude::*;
use std::path::PathBuf;
use std::time::Duration;

use super::attached_status_hooks::AttachedStatusHookWatcher;
use super::home::{HomeView, TerminalMode};
use super::status_poller::StatusUpdate;
use super::styles::Theme;
use crate::session::{get_update_settings, save_config, Config};
use crate::tmux::AvailableTools;
use crate::update::{check_for_update, UpdateInfo};

/// Minimum elapsed time between considering periodic update re-checks.
/// The main loop runs at ~20Hz; gating on this gap keeps the per-iteration
/// `get_update_settings()` config read off the hot path while still
/// re-evaluating well under any realistic `check_interval_hours` setting.
const UPDATE_CHECK_THROTTLE_GAP: Duration = Duration::from_secs(60);

/// Floor for the periodic re-check interval. The settings TUI validator
/// rejects `check_interval_hours = 0`, but a user could still land in that
/// state by hand-editing the config file. Without a floor, the periodic
/// re-check would fire once per `UPDATE_CHECK_THROTTLE_GAP` (60s) and the
/// underlying `check_for_update` cache TTL would also be zero, defeating
/// the cache and hitting GitHub on every tick. One hour is generous; users
/// who genuinely want hourly checks set `check_interval_hours = 1` and get
/// the same effect via the normal path.
const MIN_PERIODIC_RECHECK_INTERVAL: Duration = Duration::from_secs(3600);

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

pub struct App {
    home: HomeView,
    should_quit: bool,
    theme: Theme,
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
    /// True when running under Mosh (`MOSH_CONNECTION` set). Mosh mangles
    /// xterm mouse-tracking escapes, so `tui::run` skips the startup
    /// `EnableMouseCapture` and `sync_mouse_capture` must not re-enable
    /// tracking mid-session either.
    mosh_active: bool,
    /// Set by `Action::OpenStructuredView` so the async main loop can pick it
    /// up and enter the acp view (which needs `event_stream` access
    /// the sync `execute_action` can't lend out).
    #[cfg(feature = "serve")]
    pending_structured_view_open: Option<String>,
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

impl App {
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

    pub fn new(
        profile: &str,
        available_tools: AvailableTools,
        suppress_first_run_dialogs: bool,
        mosh_active: bool,
    ) -> Result<Self> {
        let no_agents = !available_tools.any_available();
        let active_profile = if profile.is_empty() {
            None // all-profiles mode
        } else {
            Some(profile.to_string())
        };
        let mut home = HomeView::new(active_profile, available_tools)?;

        // Check if we need to show welcome or changelog dialogs
        let mut config = Config::load_or_warn();

        // Load theme from config, defaulting to the `default` builtin if
        // empty so the TUI matches the web dashboard's empty-name fallback.
        let theme_name = if config.theme.name.is_empty() {
            "default"
        } else {
            &config.theme.name
        };
        let palette_mode = matches!(
            config.theme.color_mode,
            crate::session::config::ColorMode::Palette
        );
        let theme = crate::tui::styles::load_theme_with_mode(theme_name, palette_mode);
        let current_version = env!("CARGO_PKG_VERSION").to_string();

        if no_agents {
            // Show the no-agents onboarding dialog (takes priority over welcome/changelog)
            home.show_no_agents();
        } else if suppress_first_run_dialogs {
            // A startup warning will be shown by the caller; skip welcome and
            // changelog so the warning is what the user sees first, and avoid
            // overwriting a malformed config.toml with defaults via save_config.
        } else if !config.app_state.has_seen_welcome {
            home.show_intro(theme_name);
            config.app_state.has_seen_welcome = true;
            config.app_state.last_seen_version = Some(current_version);
            save_config(&config)?;
        } else if config.app_state.last_seen_version.as_deref() != Some(&current_version) {
            // Cache should already be refreshed by tui::run() before App::new
            home.show_changelog(config.app_state.last_seen_version.clone());
            config.app_state.last_seen_version = Some(current_version);
            save_config(&config)?;
        } else if !config.app_state.has_responded_to_telemetry {
            // Existing users who finished the walkthrough before telemetry
            // existed get a one-time opt-in popup. Gated behind the changelog
            // branch above (mutually exclusive in this if/else chain), so it
            // never co-renders with the changelog; and because it is a modal
            // dialog, the version update modal (opened only by an explicit
            // keypress) can't open on top of it while it is up. No save here:
            // the dialog's response handler persists the answer.
            home.show_telemetry_consent();
        }

        let dismissed_update_version = config.app_state.dismissed_update_version.clone();

        Ok(Self {
            home,
            should_quit: false,
            theme,
            needs_redraw: true,
            update_info: None,
            update_rx: None,
            update_status: None,
            update_status_rx: None,
            dismissed_update_version,
            event_stream: Some(EventStream::new()),
            // Initial state matches whatever `tui::run` did at startup: capture
            // is requested by default, but Mosh suppresses the actual escape, so
            // `mouse_captured` (live state) also factors in `mosh_active`.
            // `mouse_capture_allowed` is permission only and ignores Mosh.
            mouse_captured: crate::tui::mouse_capture_requested(&config.session) && !mosh_active,
            mouse_capture_allowed: crate::tui::mouse_capture_requested(&config.session),
            mosh_active,
            #[cfg(feature = "serve")]
            pending_structured_view_open: None,
            pending_install_version: None,
            last_installed_version_in_session: None,
        })
    }

    /// Turn xterm mouse tracking on or off to match the current view state.
    ///
    /// **Contract**: must be called after any handler that may open or close
    /// a surface counted by `HomeView::wants_text_selection`. Currently the
    /// event-loop `Event::Key` arm and the tail of `with_raw_mode_disabled`
    /// cover this; new event sources that mutate dialog state need to call
    /// this too or mouse capture will lag a frame behind reality.
    fn sync_mouse_capture(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    ) -> Result<()> {
        // Mouse capture is on by default; the Mouse Capture setting (or the
        // AOE_MOUSE_CAPTURE=0 backstop) opts out so iOS Mosh + Termius/Blink
        // use the terminal app's native scrollback for touch-scroll (Mosh
        // doesn't reliably forward mouse-tracking escapes to mobile clients).
        // Folding `mouse_capture_allowed` into `desired` (rather than an early
        // return) means flipping the setting off mid-session disables tracking
        // on the next sync instead of leaving it stuck on. `mosh_active` is
        // folded in too so a mid-session enable never emits the escape under
        // Mosh, matching the startup gate in `tui::run`.
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

    /// Draw a frame without exposing ratatui's intermediate cursor moves.
    ///
    /// The backend moves the real terminal cursor while flushing changed
    /// cells. If an IME is composing text, those transient moves can pull the
    /// candidate window toward refreshed UI such as the status list before the
    /// frame's final cursor position is restored. Synchronized update batches
    /// the frame, and hiding the cursor before the batch keeps the only visible
    /// cursor transition at ratatui's final `Frame::set_cursor_position`.
    fn draw(&mut self, terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>) -> Result<()> {
        crossterm::execute!(
            terminal.backend_mut(),
            crossterm::terminal::BeginSynchronizedUpdate
        )?;
        let draw_result = (|| -> Result<()> {
            crossterm::execute!(terminal.backend_mut(), crossterm::cursor::Hide)?;
            terminal.draw(|f| self.render(f))?;
            Ok(())
        })();
        let end_result = crossterm::execute!(
            terminal.backend_mut(),
            crossterm::terminal::EndSynchronizedUpdate
        );
        draw_result?;
        end_result?;
        Ok(())
    }

    /// Temporarily leave TUI mode, run a closure, and restore TUI mode.
    /// Drops the EventStream before the closure so child processes (tmux,
    /// editors) have exclusive access to stdin, then creates a fresh one.
    fn with_raw_mode_disabled<F, R>(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
        f: F,
    ) -> Result<R>
    where
        F: FnOnce() -> R,
    {
        crossterm::terminal::disable_raw_mode()?;
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

        let result = f();

        // Recreate the event stream with a fresh reader before re-entering
        // the event loop.
        self.event_stream = Some(EventStream::new());

        crossterm::terminal::enable_raw_mode()?;
        crossterm::execute!(
            terminal.backend_mut(),
            crossterm::terminal::EnterAlternateScreen,
            EnableBracketedPaste,
            crossterm::cursor::Hide
        )?;
        // Defer mouse-capture restore to sync_mouse_capture so we don't
        // briefly enable it only to disable again when the user returned
        // to the serve view. sync_mouse_capture itself respects the Mouse
        // Capture setting and the AOE_MOUSE_CAPTURE opt-out.
        self.sync_mouse_capture(terminal)?;
        std::io::Write::flush(terminal.backend_mut())?;

        terminal.clear()?;

        Ok(result)
    }

    fn with_attached_status_hooks<F, R>(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
        f: F,
    ) -> Result<(R, Vec<StatusUpdate>)>
    where
        F: FnOnce() -> R,
    {
        let watcher = AttachedStatusHookWatcher::start(self.home.attached_status_hook_sessions());
        let result = self.with_raw_mode_disabled(terminal, f);
        let mut attached_status_updates = Vec::new();

        if let Some(watcher) = watcher {
            attached_status_updates = watcher.stop();
        }
        self.home.reset_status_refresh();

        result.map(|result| (result, attached_status_updates))
    }

    pub fn show_startup_warning(&mut self, message: &str) {
        // Warnings preempt onboarding dialogs so the user sees the problem
        // before the intro walkthrough.
        self.home.intro_dialog = None;
        self.home.changelog_dialog = None;
        self.home.telemetry_consent_dialog = None;
        tracing::info!(target: "tui.dialog", dialog = "warning", "opening warning dialog");
        self.home.info_dialog = Some(crate::tui::dialogs::InfoDialog::sized_to_fit(
            "Warning", message,
        ));
    }

    pub fn set_theme(&mut self, name: &str) {
        // Honor the saved color_mode (Palette vs Truecolor). If we don't, a
        // SetTheme dispatched from the Settings view preview/apply flow will
        // re-load the theme with raw RGB colors, "breaking the coloration"
        // on terminals that were working with the user's palette preference
        // (Termius/mosh edge cases, 8-bit-only TTYs, etc.).
        let palette_mode = crate::session::resolve_config(
            self.home.active_profile.as_deref().unwrap_or("default"),
        )
        .map(|c| {
            matches!(
                c.theme.color_mode,
                crate::session::config::ColorMode::Palette
            )
        })
        .unwrap_or(false);
        self.theme = crate::tui::styles::load_theme_with_mode(name, palette_mode);
        self.needs_redraw = true;
    }

    pub async fn run(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    ) -> Result<()> {
        // Initial render
        terminal.clear()?;
        // Sync mouse capture before the first paint so any onboarding
        // surface that wants native drag-to-select (intro Welcome page,
        // changelog, info dialog) gets capture turned off on frame 1.
        // Otherwise the user would have to press a key first.
        self.sync_mouse_capture(terminal)?;
        self.draw(terminal)?;

        // Refresh tmux session cache
        crate::tmux::refresh_session_cache();

        // Spawn async update check at startup. The periodic re-check below
        // covers long-running sessions (#1471). `last_update_check` stays
        // `None` when the startup spawn does not fire (mode=off) so that
        // toggling the mode on later triggers a check immediately, instead
        // of waiting up to `check_interval_hours` from process launch.
        let settings = get_update_settings();
        let mut last_update_check: Option<std::time::Instant> =
            if settings.update_check_mode.is_enabled() {
                self.spawn_update_check();
                Some(std::time::Instant::now())
            } else {
                None
            };

        // SIGHUP/SIGTERM futures so we exit cleanly when the terminal
        // emulator is force-quit, preventing PTY slot leaks (#541).
        // These are polled directly inside tokio::select!, which guarantees
        // they get scheduled even when no terminal events arrive.
        #[cfg(unix)]
        let (mut sighup, mut sigterm) = {
            use tokio::signal::unix::{signal, SignalKind};
            let hup = signal(SignalKind::hangup());
            let term = signal(SignalKind::terminate());
            if let Err(ref e) = hup {
                tracing::warn!(target: "tui.input", "Failed to register SIGHUP handler: {}", e);
            }
            if let Err(ref e) = term {
                tracing::warn!(target: "tui.input", "Failed to register SIGTERM handler: {}", e);
            }
            (hup.ok(), term.ok())
        };

        // 33ms ticker (~30fps) is the steady-state refresh in live-send.
        // 16ms (60fps) was tried but produced visible tearing on
        // terminals that don't support synchronized-update escapes
        // (notably macOS Terminal.app); back-to-back ticker + post-key
        // wakes within ~1ms also doubled-up frame writes. 33ms gives
        // each frame's writes enough time to land before the next
        // frame starts, while remaining responsive enough that
        // animation looks fluid. The post-key wake below covers the
        // typing-echo case where 33ms would feel laggy.
        let mut refresh_interval = tokio::time::interval(Duration::from_millis(33));
        refresh_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // After any keystroke routed to live-send, schedule one extra
        // refresh ~15ms later (roughly the `tmux send-keys` fork plus
        // agent-echo time) so the resulting capture catches the echo
        // deterministically instead of waiting up to one full ticker
        // interval. Cleared when the wake fires; re-armed by each
        // subsequent key.
        let mut last_live_key_at: Option<std::time::Instant> = None;
        const POST_KEY_WAKE_DELAY: Duration = Duration::from_millis(15);
        // Track when the last refresh fired so the ticker arm can
        // back off if a post-key wake just ran. Without this, a key
        // pressed ~10ms before a ticker tick produces two refreshes
        // back-to-back (post-key wake at +15ms, ticker at +16ms),
        // which on a non-sync-update terminal looks like tearing:
        // the first frame's per-cell writes are still landing when
        // the second frame starts overwriting them.
        let mut last_refresh_at: Option<std::time::Instant> = None;
        const REFRESH_COOLDOWN: Duration = Duration::from_millis(15);
        let mut last_status_refresh = std::time::Instant::now();
        let mut last_disk_refresh = std::time::Instant::now();
        let mut last_spinner_redraw = std::time::Instant::now();
        let mut last_heartbeat = std::time::Instant::now();
        let mut last_presence_refresh = std::time::Instant::now();
        let mut last_session_idle_reap = std::time::Instant::now();
        // Throttle for how often the periodic block re-reads settings;
        // without this, the inner guards would re-fire on every loop
        // iteration once any time has passed, hitting the config file at
        // the 20Hz loop rate.
        let mut last_update_eval = std::time::Instant::now();
        const STATUS_REFRESH_INTERVAL: Duration = Duration::from_millis(500);
        const DISK_REFRESH_INTERVAL: Duration = Duration::from_secs(5);
        // Fastest spinner (breathe) changes every 180ms; 120ms ensures smooth animation
        const SPINNER_REDRAW_INTERVAL: Duration = Duration::from_millis(120);
        const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
        // How often to recount live TUIs for the footer indicator. Cheap dir
        // listing (a handful of entries), so a tight-ish cadence keeps the
        // "another instance appeared/left" signal responsive without disk I/O
        // on the hot render path.
        const PRESENCE_REFRESH_INTERVAL: Duration = Duration::from_secs(3);
        // How often the standalone TUI evaluates plain tmux sessions for idle
        // auto-stop (`session.auto_stop_idle_secs`, #1690). Matches the serve
        // daemon's cadence; both reapers claim under the storage lock so they
        // never double-stop a session when run side by side.
        const SESSION_IDLE_REAP_INTERVAL: Duration = Duration::from_secs(60);
        // A presence file counts as live while its mtime is within this window.
        // Larger than HEARTBEAT_INTERVAL so a couple of missed beats (busy loop,
        // brief stall) don't drop an instance; matches the push consumer.
        const PRESENCE_FRESH_WINDOW: Duration = Duration::from_secs(30);

        // Signal that the TUI is active so the web push consumer can
        // suppress notifications while the user is watching the dashboard, and
        // so other TUIs can count this instance.
        crate::session::write_tui_heartbeat();
        self.home.active_tui_count = crate::session::count_active_tuis(PRESENCE_FRESH_WINDOW);

        // Telemetry (opt-in, no-op otherwise): announce this surface on boot,
        // send an initial snapshot, then refresh it periodically and once more
        // on graceful exit. All sends are detached and swallow errors. The
        // periodic interval carries bounded jitter (4h + up to 30m) so installs
        // that boot together don't snapshot in lockstep; the boot snapshot above
        // stays immediate.
        let telemetry_snapshot_interval = crate::telemetry::snapshot_interval();
        crate::telemetry::spawn_process_start(crate::telemetry::Surface::Tui);
        self.emit_telemetry_snapshot();
        let mut last_telemetry_snapshot = std::time::Instant::now();

        loop {
            // Force full redraw if needed (e.g., after returning from tmux).
            // with_raw_mode_disabled drops and recreates the EventStream, so
            // there are no stale events to drain.
            if self.needs_redraw {
                terminal.clear()?;
                self.needs_redraw = false;
            }

            // Compute the post-key wake deadline once per iteration so
            // the select! arm doesn't have to dance with the Option.
            // `None` here becomes `pending` inside the arm.
            let post_key_deadline = last_live_key_at.map(|t| t + POST_KEY_WAKE_DELAY);
            let mut woke_via_post_key = false;
            // The capture worker notifies this when it has fresh, changed
            // pane content; the arm below wakes the loop so the new preview
            // paints without busy-polling. Cloned per iteration so the
            // select! arm doesn't borrow `self`.
            let preview_wake = self.home.preview_wake.clone();
            let mut woke_via_preview = false;

            // All event sources are polled cooperatively via tokio::select!.
            // This ensures signal futures actually get scheduled (fixing #608
            // defect 1), and that EOF from a dead tty is detected (defect 2).
            tokio::select! {
                event = self.event_stream.as_mut().expect("event_stream missing").next() => {
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
                                if burst_keys.len() >= PASTE_BURST_MIN_LEN {
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
                                        self.home.handle_paste(&paste_text);
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
                                                let hit_scroll_target = hit_diff || hit_list || hit_preview;
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
                                                        if self.home.handle_context_menu_click(mouse.column, mouse.row) {
                                                            // Click consumed by the context menu
                                                            // (item dispatched, kept open, or
                                                            // dismissed on outside-click).
                                                        } else if self.home.handle_dialog_click(mouse.column, mouse.row) {
                                                            // A modal (e.g. the telemetry consent
                                                            // popup) swallowed the click. Mirrors the
                                                            // non-burst path so dialog buttons are
                                                            // clickable even when a mouse event lands
                                                            // right after a paste/dictation burst.
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
                                // Mouse-capture state may have changed if the
                                // burst opened or closed a copy-friendly surface
                                // (info/changelog/serve dialog). Keep it in sync
                                // before the next render, matching the
                                // non-burst Event::Key arm below.
                                self.sync_mouse_capture(terminal)?;
                                if !self.needs_redraw {
                                    self.draw(terminal)?;
                                }
                                if self.should_quit {
                                    break;
                                }
                                continue;
                            }

                            self.handle_key(key, terminal).await?;
                            self.sync_mouse_capture(terminal)?;

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
                            let hit_list = self.home.hit_list(mouse.column, mouse.row);
                            let hit_preview = self.home.hit_preview(mouse.column, mouse.row);
                            let hit_diff = self.home.is_diff_open()
                                && self.home.hit_diff(mouse.column, mouse.row);
                            let hit_scroll_target = hit_diff || hit_list || hit_preview;
                            // Left-click is handled outside the unified
                            // match because it returns an `Option<Action>`
                            // (a double-click activates the session and
                            // needs to flow through `execute_action`), not
                            // a bool. The single-click selection always
                            // mutates `cursor` so we redraw unconditionally
                            // before dispatching the action.
                            //
                            // Priority order for `Down(Left)`:
                            //   1. context menu outside-click (close it)
                            //   2. modal dialog click (e.g. delete Yes/No)
                            //   3. drag-start (divider, or preview text
                            //      selection)
                            //   4. list row click (existing select/activate)
                            // A bare press on the preview seeds a 1x1
                            // PreviewSelect; `handle_drag_end` collapses it
                            // back to no selection on release if the cursor
                            // never moved.
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
                                    self.sync_mouse_capture(terminal)?;
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
                                } else if hit_diff {
                                    // The diff view file-list panel
                                    // accepts clicks to select files,
                                    // matching j/k navigation. Other
                                    // diff regions are no-op.
                                    let _ = self.home.clear_preview_selection();
                                    self.home.handle_diff_click(mouse.column, mouse.row);
                                    self.draw(terminal)?;
                                    None
                                } else {
                                    let _ = self.home.clear_preview_selection();
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
                                    // Route hover to the diff view's
                                    // file list when one is open AND
                                    // the mouse is over it; that's an
                                    // OR with the home view's own hover
                                    // (which already covers list +
                                    // overlay dialogs).
                                    let mut changed =
                                        self.home.handle_hover(mouse.column, mouse.row);
                                    if hit_diff {
                                        changed |= self
                                            .home
                                            .handle_diff_hover(mouse.column, mouse.row);
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
                                #[cfg(feature = "serve")]
                                if let Some(session_id) = self.pending_structured_view_open.take() {
                                    self.run_structured_view(&session_id, terminal).await?;
                                }
                            }
                            // Drain any Action stashed by a modal-dialog
                            // click (e.g. clicking `[Yes]` on a stop or
                            // quit confirm). The keyboard path returns
                            // these through handle_key; the click path
                            // can't, so it stashes them here.
                            if let Some(action) = self.home.pending_dialog_click_action.take() {
                                self.execute_action(action, terminal)?;
                            }
                            continue;
                        }
                        Some(Ok(Event::Paste(text))) => {
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

            // Continuous edge auto-scroll for a preview drag-select. The
            // mouse-event arm `continue`s above, so this runs on the
            // ~33ms ticker (and other wakes): while the cursor is held at
            // the pane edge, scroll one line and extend the selection so a
            // single drag can grab more than a page without depending on
            // mouse movement to fire events. No-op unless a drag is live
            // and the pointer sits at the edge.
            //
            // Request a normal (diffed) redraw via `refresh_needed`, NOT
            // `needs_redraw`: the latter forces a `terminal.clear()` at the
            // top of the loop, and clearing every ticker frame while the
            // scroll runs strobes the screen blank-then-repaint. The diffed
            // draw at the bottom of the loop repaints smoothly.
            if self.home.tick_preview_autoscroll() {
                refresh_needed = true;
                needs_full_refresh = true;
            }

            // Update-check / install-status polls can flip the
            // bottom-of-screen update bar (banner or transient toast)
            // on or off, which shifts the home view's layout. If a
            // live-send wake fires on the same iteration, the
            // preview-only fast path would paint a stale snapshot
            // whose preview rect no longer lines up with the new
            // layout. Treat any banner state change as full-refresh
            // work so the slow path rebuilds the layout AND the
            // snapshot.
            if self.poll_update_check() {
                self.needs_redraw = true;
                refresh_needed = true;
                needs_full_refresh = true;
            }
            if self.poll_update_status() {
                self.needs_redraw = true;
                refresh_needed = true;
                needs_full_refresh = true;
            }

            if last_status_refresh.elapsed() >= STATUS_REFRESH_INTERVAL {
                self.home.request_status_refresh();
                last_status_refresh = std::time::Instant::now();
            }

            if self.home.apply_status_updates() {
                refresh_needed = true;
                needs_full_refresh = true;
            }

            if self.home.apply_deletion_results() {
                refresh_needed = true;
                needs_full_refresh = true;
            }

            if self.home.apply_stop_results() {
                refresh_needed = true;
                needs_full_refresh = true;
            }

            if last_session_idle_reap.elapsed() >= SESSION_IDLE_REAP_INTERVAL {
                last_session_idle_reap = std::time::Instant::now();
                if self.reap_idle_sessions() {
                    refresh_needed = true;
                    needs_full_refresh = true;
                }
            }

            if self.home.apply_session_id_updates() {
                refresh_needed = true;
                needs_full_refresh = true;
            }

            if self.home.apply_recovery_updates() {
                refresh_needed = true;
                needs_full_refresh = true;
            }

            if let Some(session_id) = self.home.apply_creation_results() {
                self.dispatch_new_session_attach(&session_id, terminal)?;
                refresh_needed = true;
                needs_full_refresh = true;
            }

            if self.home.tick_dialog() {
                refresh_needed = true;
                needs_full_refresh = true;
            }

            // Defer the 5s disk reload while the user is in live-send.
            // The reload is on the UI thread and rebuilds the sidebar
            // tree from disk, which causes a visible hitch in the
            // preview. The user can't change session config from inside
            // live mode anyway. Leaving `last_disk_refresh` un-advanced
            // when we skip means the first tick outside live-send
            // re-checks the interval and reloads immediately if it's
            // been ≥5s since the last successful reload (so a change
            // on disk during a long live-send session is picked up on
            // exit instead of sitting stale for another 5s window).
            if last_disk_refresh.elapsed() >= DISK_REFRESH_INTERVAL && self.home.live_send.is_none()
            {
                self.home.reload()?;
                // Pick up a Settings > Interaction > Mouse Capture toggle from
                // disk and apply it now, so capture turns on/off within the
                // reload window instead of waiting for a restart.
                let profile = self.home.active_profile.as_deref().unwrap_or("default");
                let mouse_capture_allowed = crate::session::resolve_config(profile)
                    .map(|c| crate::tui::mouse_capture_requested(&c.session))
                    .unwrap_or(self.mouse_capture_allowed);
                if mouse_capture_allowed != self.mouse_capture_allowed {
                    self.mouse_capture_allowed = mouse_capture_allowed;
                    self.sync_mouse_capture(terminal)?;
                }
                last_disk_refresh = std::time::Instant::now();
                refresh_needed = true;
                needs_full_refresh = true;
            }

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
                    periodic_recheck_interval(settings.check_interval_hours),
                    self.update_rx.is_some(),
                    settings.update_check_mode.is_enabled(),
                ) {
                    self.spawn_update_check();
                    last_update_check = Some(std::time::Instant::now());
                }
            }

            // Animated spinners (rattles) need periodic redraws, but only
            // at the spinner frame rate to avoid unnecessary widget tree
            // rebuilds. Skip in live-send: the spinner lives in the
            // sidebar (which the user isn't looking at) and forcing a
            // full HomeView render every 120ms inside live mode wakes
            // the loop eight times a second to repaint a region the
            // user can't see, which only adds load on top of the
            // already-busy preview refresh.
            if last_spinner_redraw.elapsed() >= SPINNER_REDRAW_INTERVAL
                && self.home.has_animated_sessions()
                && self.home.live_send.is_none()
            {
                last_spinner_redraw = std::time::Instant::now();
                refresh_needed = true;
                needs_full_refresh = true;
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
            if self.home.live_send.is_some() || woke_via_post_key || woke_via_preview {
                refresh_needed = true;
            }

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
                && last_refresh_at
                    .map(|t| t.elapsed() < REFRESH_COOLDOWN)
                    .unwrap_or(false)
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

        self.home.apply_session_id_updates();
        self.home.cleanup_pending_creation();

        if let Err(e) = self.home.save() {
            tracing::error!(target: "tui.input", "Failed to save on quit: {}", e);
        }

        // Best-effort final snapshot on graceful exit, bounded so a dead
        // endpoint can't delay quit. Deduped against the boot/periodic snapshot
        // so a launch-then-quit with unchanged sessions doesn't post the same
        // counts twice within seconds.
        if let Some(snapshot) = self.build_telemetry_snapshot() {
            crate::telemetry::flush_snapshot_if_changed(snapshot).await;
        }

        Ok(())
    }

    /// Build a `usage_snapshot` from the current session list, or `None` when
    /// telemetry is not opted in. The TUI never hosts the web dashboard, so the
    /// `usage_seen` map is reported zeroed (a stable full key set), the
    /// per-client form-factor maps stay empty (and so omitted), the create-trend
    /// counter is left at 0, and the structured-interaction counts are empty (the
    /// `aoe serve` daemon is the surface that tracks all of those).
    fn build_telemetry_snapshot(&self) -> Option<crate::telemetry::UsageSnapshot> {
        crate::telemetry::build_usage_snapshot(
            crate::telemetry::Surface::Tui,
            self.home.instances(),
            crate::telemetry::usage_signals::zeroed(),
            0,
            // The TUI hosts no server, so it has no auth or exposure mode.
            None,
            None,
            &crate::telemetry::StructuredInteractionCounts::default(),
        )
    }

    /// Build and send a snapshot, detached. No-op when not opted in.
    fn emit_telemetry_snapshot(&self) {
        if let Some(snapshot) = self.build_telemetry_snapshot() {
            crate::telemetry::spawn_snapshot(snapshot);
        }
    }

    fn render(&mut self, frame: &mut Frame) {
        let start = std::time::Instant::now();
        if self.update_status.as_ref().is_some_and(|s| s.is_expired()) {
            self.update_status = None;
        }
        let status_text = self.update_status.as_ref().map(|s| s.text.as_str());
        // Reset before the render so a frame that skips the preview path
        // (dialog open, non-home view) reads as zero capture/parse rather
        // than leaking the previous frame's durations.
        self.home.preview_timings = Default::default();
        self.home.render(
            frame,
            frame.area(),
            &self.theme,
            self.update_info.as_ref(),
            status_text,
        );
        // Sampled trace for frame-budget diagnostics. A full-frame trace on
        // every paint would dominate the log at `default_level = trace`, so
        // we only emit for (a) frames that break the 16ms / 60fps budget and
        // (b) live-send frames, where the per-frame `tmux capture-pane` fork
        // is the latency we're profiling and individual frames usually stay
        // under 16ms. `capture_us` / `parse_us` break the frame down into the
        // capture fork vs. the `ansi-to-tui` parse; the remainder (frame_ms
        // minus those two) is the widget build + ratatui diff.
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
                capture_us = timings.capture.as_micros() as u64,
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
            // For Homebrew installs, suppress the "update available" banner
            // until the formula has caught up to the GitHub release.
            // Otherwise users see the prompt, press 'u', and hit a no-op
            // `brew upgrade` while the formula lags. The brew probes are
            // sync; offload to keep the runtime free.
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

        // Auto mode: install in the background and suppress the banner.
        // The new binary is picked up on next launch; we do not restart
        // the TUI mid-session (avoids racing tmux attaches and partial
        // writes to the binary while it is running).
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

        self.update_status = Some(UpdateStatus::transient(format!(
            "auto-updating to v{version} in background…"
        )));
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
                self.update_status = Some(UpdateStatus::transient(format!("update failed: {e}")));
                true
            }
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
                self.update_status_rx = Some(rx);
                false
            }
            Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                self.pending_install_version = None;
                self.update_status = Some(UpdateStatus::transient(
                    "update task ended unexpectedly".into(),
                ));
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
        terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    ) -> Result<()> {
        use crate::update::install::InstallMethod;

        let needs_sudo = matches!(
            &method,
            InstallMethod::Tarball { binary_path }
                if !crate::update::install::parent_is_writable(binary_path)
        );

        if matches!(method, InstallMethod::Homebrew) || needs_sudo {
            // Suspend the TUI so sudo's password prompt can use the terminal.
            self.update_status = Some(UpdateStatus::transient(format!("updating to v{version}…")));
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
                    self.update_status =
                        Some(UpdateStatus::transient(format!("update failed: {e}")));
                }
            }
        } else {
            // Background task for writable tarball installs.
            // `perform_update`'s future is !Send because its `on_progress` parameter is
            // `Option<&mut dyn FnMut(...)>` (no Send bound on the trait object), so
            // `tokio::spawn` won't accept it. A std::thread + Handle::block_on lets the
            // async I/O still use the existing tokio runtime while sidestepping the
            // Send constraint.
            self.update_status = Some(UpdateStatus::transient(format!("updating to v{version}…")));
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
    let mut config = Config::load_or_warn();
    config.app_state.dismissed_update_version = version;
    if let Err(e) = save_config(&config) {
        tracing::warn!(
            target: "update.snooze",
            error = %e,
            "failed to persist dismissed_update_version"
        );
    }
}

/// Convert `check_interval_hours` to a `Duration` for the periodic re-check,
/// clamped to a sane minimum. See `MIN_PERIODIC_RECHECK_INTERVAL`.
fn periodic_recheck_interval(check_interval_hours: u64) -> Duration {
    Duration::from_secs(check_interval_hours.saturating_mul(3600))
        .max(MIN_PERIODIC_RECHECK_INTERVAL)
}

/// Decide whether the main loop should spawn a fresh periodic update check.
/// Pulled out as a pure function so the throttle/in-flight/mode guards are
/// testable without driving the tokio runtime, the config file, or the
/// network. `elapsed = None` means no check has run yet this process, which
/// makes the first tick after the user enables update_check_mode mid-session
/// fire immediately rather than waiting up to `check_interval_hours` from
/// process launch. `interval` is the value produced by
/// `periodic_recheck_interval`.
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

impl App {
    async fn handle_key(
        &mut self,
        key: KeyEvent,
        terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    ) -> Result<()> {
        // Global keybindings
        match (key.code, key.modifiers) {
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
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
            // Ctrl+x dismisses the update bar / status toast. Gated on
            // something being visible AND no dialog open so it doesn't fire
            // during dialog input. The dismissed version is persisted to
            // `app_state.dismissed_update_version` so the snooze survives
            // restarts; the banner returns automatically when a newer
            // release ships (per #1140).
            //
            // No `needs_redraw = true` here: that forces a `terminal.clear()`
            // before the next event arrives, so the whole screen blanks for
            // a beat (visible flash). Ratatui's diff renderer handles the
            // 1-row layout shrink on the next normal draw.
            (KeyCode::Char('x'), KeyModifiers::CONTROL)
                if (self.update_info.is_some() || self.update_status.is_some())
                    && !self.home.has_dialog() =>
            {
                if let Some(info) = self.update_info.as_ref() {
                    let v = info.latest_version.clone();
                    self.dismissed_update_version = Some(v.clone());
                    persist_dismissed_update_version(Some(v));
                }
                self.update_info = None;
                self.update_status = None;
                return Ok(());
            }
            _ => {}
        }

        if let Some(action) = self.home.handle_key(key, self.update_info.as_ref()) {
            self.execute_action(action, terminal)?;
        }

        #[cfg(feature = "serve")]
        if let Some(session_id) = self.pending_structured_view_open.take() {
            self.run_structured_view(&session_id, terminal).await?;
        }

        Ok(())
    }

    #[cfg(feature = "serve")]
    async fn run_structured_view(
        &mut self,
        session_id: &str,
        terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    ) -> Result<()> {
        // The acp view borrows the EventStream so it can drive its
        // own tokio::select! loop. Pull it out for the duration of the
        // call; restore on return.
        let mut stream = match self.event_stream.take() {
            Some(s) => s,
            None => return Ok(()),
        };
        let result =
            crate::tui::structured_view::run(terminal, &mut stream, &self.theme, session_id).await;
        self.event_stream = Some(stream);
        // Forcing a full redraw on return so the home screen redraws
        // any cells the acp view painted over.
        self.needs_redraw = true;
        terminal.clear()?;
        if let Err(e) = result {
            self.update_status = Some(UpdateStatus::transient(format!("acp closed: {e}")));
        }
        Ok(())
    }

    /// Auto-stop plain tmux sessions idle past `session.auto_stop_idle_secs`
    /// (#1690). Runs on a 60s gate from the main loop. Each candidate is
    /// claimed under the per-profile storage lock (so a co-running `aoe serve`
    /// cannot double-stop it), marked `Stopped` in memory, then handed to the
    /// background `StopPoller`; the result is reconciled by `apply_stop_results`
    /// like a manual stop. Returns true if any session was reaped.
    fn reap_idle_sessions(&mut self) -> bool {
        // Live attach state; on a tmux query failure skip this pass rather
        // than risk reaping a session the user is attached to.
        let Ok(attached) = crate::tmux::attached_session_names() else {
            return false;
        };
        let now = chrono::Utc::now();
        let candidates = crate::session::idle_reap::idle_reap_candidates(
            self.home.instances(),
            now,
            &attached,
            |profile| {
                crate::session::profile_config::resolve_config_or_warn(profile)
                    .session
                    .auto_stop_idle_secs
            },
        );
        let mut reaped = false;
        for cand in candidates {
            match crate::session::idle_reap::claim_idle_stop(
                &cand.profile,
                &cand.session_id,
                now,
                cand.threshold_secs,
            ) {
                Ok(Some(instance)) => {
                    // Mirror Action::StopSession: the claim already persisted
                    // `Stopped`; reassert it in memory and run the kill off the
                    // UI thread so a sandbox `docker stop` cannot freeze the TUI.
                    self.home
                        .set_instance_status(&cand.session_id, crate::session::Status::Stopped);
                    self.home
                        .stop_poller
                        .request_stop(crate::tui::stop_poller::StopRequest {
                            session_id: cand.session_id.clone(),
                            instance,
                        });
                    tracing::info!(
                        target: "tui.idle_reap",
                        session = %cand.session_id,
                        profile = %cand.profile,
                        threshold_secs = cand.threshold_secs,
                        "auto-stopped idle tmux session",
                    );
                    reaped = true;
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(
                        target: "tui.idle_reap",
                        session = %cand.session_id,
                        error = %e,
                        "idle auto-stop claim failed",
                    );
                }
            }
        }
        if reaped {
            if let Err(e) = self.home.save() {
                tracing::error!(target: "tui.idle_reap", "failed to save after idle reap: {e}");
            }
        }
        reaped
    }

    fn execute_action(
        &mut self,
        action: Action,
        terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    ) -> Result<()> {
        match action {
            Action::Quit => self.should_quit = true,
            Action::AttachSession(id) => {
                self.attach_session(&id, terminal)?;
            }
            Action::AttachAfterCreate(id) => {
                self.dispatch_new_session_attach(&id, terminal)?;
            }
            Action::AttachTerminal(id, mode) => {
                self.attach_terminal(&id, mode, terminal)?;
            }
            Action::EditFile(path) => {
                self.edit_file(&path, terminal)?;
            }
            Action::StopSession(id) => {
                if let Some(inst) = self.home.get_instance(&id) {
                    // Run the stop on a background thread: `inst.stop()` calls
                    // `docker stop` for sandboxed sessions, which can block for
                    // the container's grace period (~10s) and would otherwise
                    // freeze the TUI (issue #1496). Set Stopped immediately so
                    // the status poller won't override to Error while the stop
                    // is in flight; the result is applied in the main loop via
                    // `apply_stop_results`.
                    let request = crate::tui::stop_poller::StopRequest {
                        session_id: id.clone(),
                        instance: inst.clone(),
                    };
                    self.home
                        .set_instance_status(&id, crate::session::Status::Stopped);
                    self.home.save()?;
                    self.home.stop_poller.request_stop(request);
                }
            }
            Action::SetTheme(name) => {
                self.set_theme(&name);
            }
            Action::SpawnUpdate(method, version) => {
                if self.update_status_rx.is_some() {
                    self.update_status =
                        Some(UpdateStatus::transient("update already in progress".into()));
                    return Ok(());
                }
                self.spawn_update(method, version, terminal)?;
            }
            Action::SetTransientStatus(text) => {
                self.update_status = Some(UpdateStatus::transient(text));
            }
            Action::SendMessage(id, message) => {
                // Flip the row to Starting and show a toast so the user has
                // visible feedback during ensure_pane_ready, which can take
                // several seconds on a cold-start sandboxed session (Docker
                // pull) or while the readiness loop waits for an agent
                // splash to clear. The status poller will correct the row
                // back to the real state after we return.
                self.home
                    .set_instance_status(&id, crate::session::Status::Starting);
                self.update_status = Some(UpdateStatus::transient("Reviving session...".into()));
                self.draw(terminal)?;
                let stale_sid = self.home.execute_send_message(&id, &message);
                match stale_sid {
                    Some(sid) => {
                        self.update_status = Some(UpdateStatus::transient(format!(
                            "Resume failed for sid {sid}; sent to fresh session (history not loaded)"
                        )));
                    }
                    None => {
                        self.update_status = None;
                    }
                }
            }
            Action::EnterLiveSend(id) => {
                // Same revive flow as SendMessage so cold-start (Docker,
                // agent splash) gives the user "Reviving..." feedback.
                // After the pane is ready, install the live-send state on
                // HomeView so the next key event routes through the live
                // handler instead of the normal action dispatch.
                self.home
                    .set_instance_status(&id, crate::session::Status::Starting);
                self.update_status = Some(UpdateStatus::transient("Reviving session...".into()));
                self.draw(terminal)?;
                let outcome = self.home.prepare_live_send(&id);
                // Settle the toast to its final state BEFORE the sync resize
                // and redraw, so HomeView's cached `preview_pane_area`
                // matches the geometry the user will see for the next
                // several frames. Otherwise the toast row that was on screen
                // during `prepare_live_send` would make the preview pane one
                // row shorter than post-toast, the sync resize would target
                // the smaller pane, and the first capture would render
                // shifted up.
                self.update_status = match &outcome {
                    Ok(Some(sid)) => Some(UpdateStatus::transient(format!(
                        "Resume failed for sid {sid}; live-send sent to a fresh pane (history not loaded)"
                    ))),
                    // On clean ready, drop the toast entirely. On Err the
                    // info_dialog already carries the failure detail, so the
                    // transient toast just gets in the way.
                    Ok(None) | Err(()) => None,
                };
                if outcome.is_ok() {
                    self.draw(terminal)?;
                    self.home.finalize_live_send_resize();
                }
            }
            Action::AttachToolSession(id, tool_name) => {
                self.attach_tool_session(&id, &tool_name, terminal)?;
            }
            #[cfg(feature = "serve")]
            Action::OpenStructuredView(id) => {
                // Stash for the async main loop. The acp view needs
                // `event_stream` access that this sync handler can't
                // lend; the loop picks `pending_structured_view_open` up after
                // we return.
                self.pending_structured_view_open = Some(id);
            }
        }
        Ok(())
    }

    /// Route a freshly-created session through the user's
    /// `new_session_attach_mode` setting. Shared by both creation paths
    /// (synchronous `Action::AttachAfterCreate` and the async branch in
    /// the main loop's `apply_creation_results` handler) so the setting
    /// applies regardless of which one fired.
    ///
    /// Acp sessions return `None` from the resolver and fall through
    /// to `attach_session`, which already no-ops for acp. Same for
    /// missing-instance race conditions: better to do the tmux-attach
    /// fallback than silently swallow the new session.
    fn dispatch_new_session_attach(
        &mut self,
        session_id: &str,
        terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    ) -> Result<()> {
        let mode = self.home.new_session_attach_mode(session_id);
        tracing::debug!(target: "tui.input",
            session_id = %session_id,
            mode = ?mode,
            "new session created; dispatching attach mode"
        );
        match mode {
            Some(crate::session::NewSessionAttachMode::LiveSend) => {
                self.execute_action(Action::EnterLiveSend(session_id.to_string()), terminal)
            }
            Some(crate::session::NewSessionAttachMode::Tmux) | None => {
                self.attach_session(session_id, terminal)
            }
        }
    }

    fn attach_session(
        &mut self,
        session_id: &str,
        terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    ) -> Result<()> {
        let instance = match self.home.get_instance(session_id) {
            Some(inst) => inst.clone(),
            None => return Ok(()),
        };

        // Acp-mode sessions are not backed by tmux. The Enter
        // handler in `home::input` already short-circuits with a
        // transient toast pointing the user at the web dashboard;
        // this function still gets called from `apply_creation_results`
        // after `aoe add --launch`, so guard here too. Falling through
        // would attempt a tmux attach against a non-existent pane.
        if instance.is_structured() {
            let _ = terminal;
            return Ok(());
        }

        let tmux_session = instance.tmux_session()?;

        // Decide whether to restart: if hook status is available or the instance
        // uses a custom command, trust that over shell detection. Wrapper scripts
        // (Devbox, version managers, custom command overrides) run agents via a
        // shell process, so is_pane_running_shell() returns true even when the
        // agent is healthy.
        let exists = tmux_session.exists();
        let pane_dead = if exists {
            tmux_session.is_pane_dead()
        } else {
            false
        };
        let needs_restart = if !exists || pane_dead {
            true
        } else if crate::hooks::read_hook_status(&instance.id).is_some() {
            // Hook status is tracking this session; shell detection is unreliable
            false
        } else if instance.has_command_override() {
            // Custom command overrides run agents through wrapper scripts that
            // appear as shell processes to tmux. Don't restart based on shell
            // detection. (extra_args alone should not suppress this check.)
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
        if needs_restart {
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

                        // Persist the "seen" flag so it only shows once
                        let mut config = config;
                        config.app_state.has_seen_custom_instruction_warning = true;
                        save_config(&config)?;

                        return Ok(());
                    }
                }
            }

            // Get terminal size to pass to tmux session creation
            // This ensures the session starts at the correct size instead of 80x24 default
            let size = crate::terminal::get_size();

            // Skip on_launch hooks if they already ran in the background creation poller
            let skip_on_launch = self.home.take_on_launch_hooks_ran(session_id);

            self.home
                .set_instance_status(session_id, crate::session::Status::Starting);
            match self
                .home
                .restart_instance_with_size_opts(session_id, size, skip_on_launch)
            {
                Err(e) => {
                    let err_str = e.to_string();
                    self.home
                        .set_instance_error(session_id, Some(err_str.clone()));
                    self.home
                        .set_instance_status(session_id, crate::session::Status::Error);
                    // Without a toast, set_instance_error + Status::Error are
                    // invisible to the user: the TUI redraws on home as if Enter
                    // did nothing. Toast text is single-line; the bar truncates
                    // at terminal width without us needing to pre-clip.
                    self.update_status = Some(UpdateStatus::transient(format!(
                        "restart failed: {err_str}"
                    )));
                    return Ok(());
                }
                Ok(crate::session::StartOutcome::Restarted { stale_sid }) => {
                    self.update_status = Some(UpdateStatus::transient(format!(
                        "Resume failed for sid {stale_sid}; started fresh (history not loaded)"
                    )));
                }
                Ok(_) => {}
            }
            self.home.set_instance_error(session_id, None);
        }

        let tmux_session = match self.home.get_instance(session_id) {
            Some(inst) => inst.tmux_session()?,
            None => return Ok(()),
        };
        // The non-live preview may have left the window pinned to manual
        // sizing at the (smaller) preview dimensions. Restore `window-size
        // latest` so the attaching client resizes it to the full terminal,
        // and drop the preview-resize dedup so the next render re-asserts the
        // preview geometry against the now-grown window instead of leaving the
        // top clipped.
        tmux_session.reset_size_to_latest_client();
        self.home.clear_preview_pane_sync();
        let (attach_result, attached_status_updates) =
            self.with_attached_status_hooks(terminal, || tmux_session.attach())?;

        self.needs_redraw = true;
        crate::tmux::refresh_session_cache();
        self.home.reload()?;
        self.home
            .apply_status_updates_without_hooks(attached_status_updates);
        self.home.stamp_last_accessed(session_id);
        // Persist so the attach-return bump survives aoe restart. Same
        // reasoning as the send-message path in home/input.rs: without a
        // save() here the aging signal collapses back to startup timestamps
        // on next launch.
        if let Err(e) = self.home.save() {
            tracing::error!("Failed to save after attach-return: {}", e);
        }
        // In Attention sort, jump cursor to the top-attention row instead of
        // pinning it to the session we just came from; that session has
        // typically been bumped down a tier (Waiting → Running) and the next
        // item needing attention is now at row 0.
        if self.home.sort_order() == crate::session::config::SortOrder::Attention {
            self.home.select_top_attention(Some(session_id));
        } else {
            self.home.select_session_by_id(session_id);
        }

        if let Err(e) = attach_result {
            tracing::warn!(target: "tui.input", "tmux attach returned error: {}", e);
        }

        Ok(())
    }

    fn attach_terminal(
        &mut self,
        session_id: &str,
        mode: TerminalMode,
        terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    ) -> Result<()> {
        let instance = match self.home.get_instance(session_id) {
            Some(inst) => inst.clone(),
            None => return Ok(()),
        };

        // Get terminal size to pass to tmux session creation
        let size = crate::terminal::get_size();

        // Prepare the tmux session before leaving TUI mode
        let attach_fn: Box<dyn FnOnce() -> Result<()>> = match mode {
            TerminalMode::Container if instance.is_sandboxed() => {
                let container_session = instance.container_terminal_tmux_session()?;
                if !container_session.exists() || container_session.is_pane_dead() {
                    if container_session.exists() {
                        let _ = container_session.kill();
                    }
                    if let Err(e) = self
                        .home
                        .start_container_terminal_for_instance_with_size(session_id, size)
                    {
                        self.home
                            .set_instance_error(session_id, Some(e.to_string()));
                        return Ok(());
                    }
                }
                Box::new(move || container_session.attach())
            }
            _ => {
                let terminal_session = instance.terminal_tmux_session()?;
                if !terminal_session.exists() || terminal_session.is_pane_dead() {
                    if terminal_session.exists() {
                        let _ = terminal_session.kill();
                    }
                    if let Err(e) = self
                        .home
                        .start_terminal_for_instance_with_size(session_id, size)
                    {
                        self.home
                            .set_instance_error(session_id, Some(e.to_string()));
                        return Ok(());
                    }
                }
                Box::new(move || terminal_session.attach())
            }
        };

        let (attach_result, attached_status_updates) =
            self.with_attached_status_hooks(terminal, attach_fn)?;

        self.needs_redraw = true;
        crate::tmux::refresh_session_cache();
        self.home.reload()?;
        self.home
            .apply_status_updates_without_hooks(attached_status_updates);
        if self.home.sort_order() == crate::session::config::SortOrder::Attention {
            self.home.select_top_attention(Some(session_id));
        } else {
            self.home.select_session_by_id(session_id);
        }

        if let Err(e) = attach_result {
            tracing::warn!(target: "tui.input", "tmux terminal attach returned error: {}", e);
        }

        Ok(())
    }

    fn attach_tool_session(
        &mut self,
        session_id: &str,
        tool_name: &str,
        terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    ) -> Result<()> {
        let instance = match self.home.get_instance(session_id) {
            Some(inst) => inst.clone(),
            None => return Ok(()),
        };

        let tool_config = match self.home.tool_configs.get(tool_name) {
            Some(tc) => tc.clone(),
            None => return Ok(()),
        };

        if tool_config.command.is_empty() {
            self.home.set_instance_error(
                session_id,
                Some(format!("Tool '{}' has no command configured", tool_name)),
            );
            return Ok(());
        }

        let size = crate::terminal::get_size();
        let tool_session = crate::tmux::ToolSession::new(&instance.id, &instance.title, tool_name);

        if !tool_session.exists() || tool_session.is_pane_dead() {
            if tool_session.exists() {
                let _ = tool_session.kill();
            }
            if let Err(e) =
                tool_session.create_with_size(&instance.project_path, &tool_config.command, size)
            {
                self.home
                    .set_instance_error(session_id, Some(e.to_string()));
                return Ok(());
            }
        }

        let branch = instance
            .worktree_info
            .as_ref()
            .map(|w| w.branch.as_str())
            .or_else(|| instance.workspace_info.as_ref().map(|w| w.branch.as_str()));
        crate::tmux::status_bar::apply_all_tmux_options(
            tool_session.session_name(),
            &format!("{} ({})", instance.title, tool_name),
            branch,
            None,
        );

        let attach_fn: Box<dyn FnOnce() -> Result<()>> = Box::new(move || tool_session.attach());
        let (attach_result, attached_status_updates) =
            self.with_attached_status_hooks(terminal, attach_fn)?;

        self.needs_redraw = true;
        crate::tmux::refresh_session_cache();
        self.home.reload()?;
        self.home
            .apply_status_updates_without_hooks(attached_status_updates);
        self.home.select_session_by_id(session_id);

        if let Err(e) = attach_result {
            tracing::warn!(
                "tmux tool session '{}' attach returned error: {}",
                tool_name,
                e
            );
        }

        Ok(())
    }

    fn edit_file(
        &mut self,
        path: &std::path::Path,
        terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    ) -> Result<()> {
        // Determine which editor to use (prefer vim, fall back to nano)
        let editor = std::env::var("EDITOR")
            .ok()
            .or_else(|| {
                // Check if vim is available
                if std::process::Command::new("vim")
                    .arg("--version")
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .is_ok()
                {
                    Some("vim".to_string())
                } else if std::process::Command::new("nano")
                    .arg("--version")
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .is_ok()
                {
                    Some("nano".to_string())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| "vim".to_string());

        let path = path.to_owned();
        let editor_clone = editor.clone();
        let status = self.with_raw_mode_disabled(terminal, move || {
            std::process::Command::new(&editor_clone)
                .arg(&path)
                .status()
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
    /// Attach to a session that was just created via the synchronous
    /// create path (no sandbox, no hooks, no worktree). Routes through
    /// the same `new_session_attach_mode` dispatch as the async path's
    /// `apply_creation_results` so the user's "live mode by default"
    /// setting applies in both cases. `AttachSession` deliberately
    /// bypasses the setting because pressing Enter on a session row is
    /// the user's explicit ask for a tmux attach.
    AttachAfterCreate(String),
    /// Attach to a tool session (lazygit, yazi, etc.) for the given agent
    /// session. The tool_name indexes into Config.tools.
    AttachToolSession(String, String),
    /// Open the native acp view for `session_id`. The action handler
    /// stashes the id in `pending_structured_view_open`; the main loop drains it
    /// after `execute_action` returns and runs the async acp loop
    /// against the borrowed terminal + event stream.
    #[cfg(feature = "serve")]
    OpenStructuredView(String),
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // full `check_interval_hours` from process launch.
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
    fn periodic_recheck_interval_honors_user_setting() {
        assert_eq!(
            periodic_recheck_interval(24),
            Duration::from_secs(24 * 3600)
        );
        assert_eq!(
            periodic_recheck_interval(168),
            Duration::from_secs(168 * 3600)
        );
    }

    #[test]
    fn periodic_recheck_interval_floors_zero_to_minimum() {
        // The settings TUI rejects 0, but a hand-edited config could land
        // here. Without the floor, a 0-hour interval combined with the 0-hour
        // cache TTL would hit GitHub on every throttle-gap tick (~60s).
        assert_eq!(periodic_recheck_interval(0), MIN_PERIODIC_RECHECK_INTERVAL);
    }

    #[test]
    fn periodic_recheck_interval_does_not_overflow() {
        // `saturating_mul` keeps `u64::MAX` hours from wrapping. The result
        // is "effectively never re-check" rather than a panic.
        let _ = periodic_recheck_interval(u64::MAX);
    }

    #[test]
    fn periodic_recheck_fires_at_interval_boundary() {
        // `>=`, not `>`. A user with `check_interval_hours = 1` should get the
        // tick at the 1-hour mark, not 1h + epsilon.
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
