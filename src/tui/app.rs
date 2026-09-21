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

use super::attached_status_hooks::AttachedStatusHookWatcher;
use super::home::{HomeView, TerminalMode};
use super::status_poller::StatusUpdate;
use super::styles::Theme;
use crate::containers::image_update::ImageUpdate;
use crate::session::{get_update_settings, update_app_state, Config};
use crate::tmux::AvailableTools;
use crate::update::{check_for_update, UpdateInfo};

/// Gap between periodic update re-check evaluations, keeping the config read off the ~20Hz loop.
const UPDATE_CHECK_THROTTLE_GAP: Duration = Duration::from_secs(60);

const PERIODIC_RECHECK_INTERVAL: Duration =
    Duration::from_secs(crate::update::UPDATE_CHECK_INTERVAL_HOURS * 3600);

/// Inter-key gap under which printable keys join a paste burst. Mosh strips
/// bracketed-paste markers, so pastes arrive as a tight stream of key events.
const PASTE_BURST_INTER_KEY_MS: u64 = 5;

/// Shorter bursts are replayed as individual keys so typing isn't mistaken for a paste.
const PASTE_BURST_MIN_LEN: usize = 3;

/// Session creates since the last confirmed telemetry snapshot send.
static TUI_SESSION_CREATES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

fn reported_session_creates() -> u32 {
    TUI_SESSION_CREATES.load(std::sync::atomic::Ordering::Relaxed)
}

/// Subtract exactly what a confirmed snapshot reported, so creates that landed
/// during the send roll into the next snapshot.
fn clear_reported_session_creates(reported: u32, outcome: crate::telemetry::SendOutcome) {
    if reported == 0 || outcome != crate::telemetry::SendOutcome::Sent {
        return;
    }
    use std::sync::atomic::Ordering;
    let _ = TUI_SESSION_CREATES.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_sub(reported))
    });
}

pub(super) fn record_session_create() {
    TUI_SESSION_CREATES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(test)]
pub(crate) fn session_create_count_for_test() -> u32 {
    reported_session_creates()
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

pub type TuiBackend = crate::tui::hyperlink::HyperlinkBackend<std::io::Stdout>;

pub struct App {
    home: HomeView,
    should_quit: bool,
    theme: Theme,
    /// Guards `set_theme`: the config watcher re-dispatches the theme on every
    /// save, and a needless apply forces a flickering full clear.
    theme_name: String,
    theme_palette_mode: bool,
    needs_redraw: bool,
    update_info: Option<UpdateInfo>,
    update_rx: Option<tokio::sync::oneshot::Receiver<anyhow::Result<UpdateInfo>>>,
    update_status: Option<UpdateStatus>,
    update_status_rx: Option<tokio::sync::oneshot::Receiver<anyhow::Result<()>>>,
    /// Snoozed via Ctrl+x until a newer release ships.
    dismissed_update_version: Option<String>,
    image_update: Option<ImageUpdate>,
    image_update_rx: Option<tokio::sync::oneshot::Receiver<anyhow::Result<Option<ImageUpdate>>>>,
    image_pull_rx: Option<tokio::sync::oneshot::Receiver<anyhow::Result<()>>>,
    /// Snoozed via Ctrl+x while the registry still resolves to this digest.
    dismissed_image_digest: Option<String>,
    /// Optional so it can be dropped before spawning children: its stdin
    /// reader thread would compete with `tmux attach`.
    event_stream: Option<EventStream>,
    /// Whether xterm mouse tracking is engaged right now.
    mouse_captured: bool,
    /// Whether config permits mouse tracking at all.
    mouse_capture_allowed: bool,
    host_title: super::host_title::HostTitleTracker,
    /// Mosh mangles mouse-tracking escapes, so capture is never enabled under it.
    mosh_active: bool,
    // Actions that need the async loop, picked up after `execute_action`.
    pending_structured_view_open: Option<String>,
    pending_view_switch: Option<String>,
    pending_daemon_start_open: Option<String>,
    pending_smart_rename: Option<String>,
    /// Debounce for structured preview-on-select, so fast navigation doesn't
    /// connect a WebSocket per keystroke.
    preview_mount_pending: Option<(String, std::time::Instant)>,
    /// Cleared on failure so the user can retry.
    pending_install_version: Option<String>,
    /// The running binary keeps its old version until restart, so this stops
    /// periodic re-checks from re-offering a release we already installed.
    last_installed_version_in_session: Option<String>,
}

/// Previous version when the changelog should be shown.
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

/// Ignores SIGINT/SIGQUIT while alive. With raw mode off for a child process,
/// Ctrl+C would otherwise kill aoe and every session it manages.
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

impl App {
    /// Printable ASCII or Enter with no modifier other than Shift. Enter is
    /// included so newlines inside a Mosh-stripped paste stay in the burst.
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

    fn burst_char_for(key: &KeyEvent) -> Option<char> {
        match key.code {
            KeyCode::Char(c) => Some(c),
            KeyCode::Enter => Some('\n'),
            _ => None,
        }
    }

    /// A held key repeats the same event with paste-like timing; keep that on
    /// the normal input path so held `j`/`k` still scroll.
    fn is_auto_repeat_burst(keys: &[KeyEvent]) -> bool {
        let Some(first) = keys.first() else {
            return false;
        };
        keys.iter()
            .skip(1)
            .all(|key| key.code == first.code && key.modifiers == first.modifiers)
    }

    /// Peel a trailing Enter off a burst so it is replayed as Submit rather
    /// than inserted as a literal newline. Embedded newlines stay in the paste.
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
            None
        } else {
            Some(profile.to_string())
        };
        let mut home = HomeView::new(active_profile, available_tools, file_watch)?;

        let config = Config::load_or_warn();
        // Theme is a global preference, never profile-merged.
        let theme_name = config.effective_theme_name();
        let palette_mode = config.theme_palette_mode();
        let theme = crate::tui::styles::load_theme_with_mode(&theme_name, palette_mode);
        let current_version = env!("CARGO_PKG_VERSION").to_string();

        if no_agents {
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

    /// Must run after any handler that may open or close a surface counted by
    /// `HomeView::wants_text_selection`.
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

    /// Write OSC 0 after the frame, so it can't interleave with OSC 8 runs.
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

    /// Draw inside a synchronized update. The cursor is hidden first so an IME
    /// candidate window isn't dragged by transient cursor moves.
    fn draw(&mut self, terminal: &mut Terminal<TuiBackend>) -> Result<()> {
        // A visible caret (active embedded view, or live-send without an
        // overlay) strobes if hidden before every redraw.
        let embedded_active = self
            .home
            .structured_preview
            .as_ref()
            .is_some_and(|v| v.is_active());
        let skip_hide = embedded_active
            || (self.home.live_send.is_some() && !self.home.has_non_live_send_overlay());
        // Queue, never execute: flushing the opener alone would put the widget
        // build inside the bracket and freeze the display for that time.
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

    /// Leave TUI mode, run `f`, and restore it. The event stream is dropped so
    /// children get exclusive stdin.
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
        self.event_stream.take();

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
        #[cfg(unix)]
        let _ = crossterm::execute!(
            terminal.backend_mut(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES),
        );
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

    fn with_attached_status_hooks<F, R>(
        &mut self,
        terminal: &mut Terminal<TuiBackend>,
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
        if (self.theme_name.as_str(), self.theme_palette_mode) == (name, palette_mode) {
            return;
        }
        self.theme = crate::tui::styles::load_theme_with_mode(name, palette_mode);
        self.theme_name = name.to_string();
        self.theme_palette_mode = palette_mode;
        self.needs_redraw = true;
    }

    pub async fn run(&mut self, terminal: &mut Terminal<TuiBackend>) -> Result<()> {
        // Display snapshots are refreshed off the paint thread. Don't warm the
        // cache here: startup must paint before any tmux deadline.
        crate::tmux::spawn_snapshot_poller();

        crate::tui::clear_terminal(terminal)?;
        // Before the first paint, so onboarding surfaces get native selection on frame 1.
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
        let mut last_status_refresh = std::time::Instant::now();
        let mut last_metrics_sample = std::time::Instant::now();
        let mut last_session_feed_refresh = std::time::Instant::now();
        let mut last_disk_refresh = std::time::Instant::now();
        let mut full_heartbeat_deferred = false;
        let mut last_spinner_redraw = std::time::Instant::now();
        let mut last_heartbeat = std::time::Instant::now();
        let mut last_presence_refresh = std::time::Instant::now();
        let mut last_session_idle_reap = std::time::Instant::now();
        let mut last_update_eval = std::time::Instant::now();
        const STATUS_REFRESH_INTERVAL: Duration = Duration::from_millis(500);
        // Structured rows cost the daemon SQLite lookups per request.
        const SESSION_FEED_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
        const DISK_REFRESH_INTERVAL: Duration = Duration::from_secs(5);
        const METRICS_SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
        // Fastest spinner (breathe) changes every 180ms.
        const SPINNER_REDRAW_INTERVAL: Duration = Duration::from_millis(120);
        const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
        const PRESENCE_REFRESH_INTERVAL: Duration = Duration::from_secs(3);
        // Auto-stop for idle plain tmux sessions; the storage lock prevents
        // double stops when serve runs its own reaper.
        const SESSION_IDLE_REAP_INTERVAL: Duration = Duration::from_secs(60);
        // Wider than the heartbeat so a couple of missed beats don't drop an instance.
        const PRESENCE_FRESH_WINDOW: Duration = Duration::from_secs(30);

        crate::session::write_tui_heartbeat();
        crate::session::write_tui_activity();
        self.home.active_tui_count = crate::session::count_active_tuis(PRESENCE_FRESH_WINDOW);

        // Telemetry is opt-in; sends are detached and swallow errors.
        let telemetry_snapshot_interval = crate::telemetry::snapshot_interval();
        crate::telemetry::spawn_process_start(crate::telemetry::Surface::Tui);
        self.emit_telemetry_snapshot();
        let mut last_telemetry_snapshot = std::time::Instant::now();

        loop {
            if self.needs_redraw {
                crate::tui::clear_terminal(terminal)?;
                self.needs_redraw = false;
            }

            let post_key_deadline = last_live_key_at.map(|t| t + POST_KEY_WAKE_DELAY);
            let mut woke_via_post_key = false;
            // The capture worker notifies on changed pane content.
            let preview_wake = self.home.preview_wake.clone();
            let mut woke_via_preview = false;

            // True for a preview too: it streams into the pane.
            let embedded_mounted = self.home.structured_preview.is_some();

            tokio::select! {
                event = self.event_stream.as_mut().expect("event_stream missing").next() => {
                    match event {
                        Some(Ok(Event::Key(key))) => {
                            // Terminals reporting releases would double-fire every handler.
                            if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                                continue;
                            }
                            crate::session::write_tui_activity();
                            // Mosh strips bracketed-paste markers, so pasted text
                            // arrives as individual keys that would fire shortcuts.
                            // Only for surfaces that accept pastes; others would
                            // strand the text in `pending_paste`.
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
                                    let (paste_text, trailing_enter) =
                                        Self::split_trailing_enter(&burst_str, &burst_keys);
                                    if !paste_text.is_empty() {
                                        tracing::debug!(target: "tui.input",
                                            "paste-burst: routed {} chars via handle_paste (chars={:?})",
                                            paste_text.len(), paste_text
                                        );
                                        // Only an ACTIVE structured view owns text input.
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
                                            Event::Mouse(mouse) => {
                                                let hit_list = self.home.hit_list(mouse.column, mouse.row);
                                                let hit_preview = self.home.hit_preview(mouse.column, mouse.row);
                                                let hit_diff = self.home.is_diff_open()
                                                    && self.home.hit_diff(mouse.column, mouse.row);
                                                let hit_scroll_target = hit_diff
                                                    || hit_list
                                                    || hit_preview
                                                    || self.home.is_settings_open();
                                                match mouse.kind {
                                                    MouseEventKind::ScrollUp if hit_scroll_target => { self.home.handle_scroll_up(mouse.column, mouse.row); }
                                                    MouseEventKind::ScrollDown if hit_scroll_target => { self.home.handle_scroll_down(mouse.column, mouse.row); }
                                                    // Clicks only select mid-burst:
                                                    // activation would reattach the
                                                    // terminal while draining keys.
                                                    MouseEventKind::Down(MouseButton::Left) => {
                                                        if self.home.handle_context_menu_click(mouse.column, mouse.row)
                                                            || self.home.handle_dialog_click(mouse.column, mouse.row)
                                                            || self.home.handle_sidebar_collapse_click(mouse.column, mouse.row)
                                                            || self.home.handle_diagnostics_click(mouse.column, mouse.row)
                                                        {
                                                        } else if self.home.handle_tips_badge_click(mouse.column, mouse.row) {
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

                            let live_after = self.home.live_send.is_some();
                            if live_after {
                                last_live_key_at = Some(std::time::Instant::now());
                            }

                            // In live-send the key has not reached tmux yet; the
                            // post-key wake paints the echo instead.
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
                            // Only the wheel and clicks that enter or leave the
                            // pane are claimed; drags and double-clicks go through
                            // home like terminal previews.
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
                            // Footer buttons replay their shortcut. Returns None
                            // while an overlay is open.
                            if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
                                if let Some(key) =
                                    self.home.footer_button_at(mouse.column, mouse.row)
                                {
                                    let _ = self.home.clear_preview_selection();
                                    self.handle_key(key, terminal).await?;
                                    self.sync_mouse_capture(terminal)?;
                                    if !self.needs_redraw {
                                        self.draw(terminal)?;
                                    }
                                    if self.should_quit {
                                        break;
                                    }
                                    continue;
                                }
                            }
                            // Checked before forwarding so the agent doesn't swallow the second press.
                            if let Some(action) = self.home.preview_double_click_action(
                                mouse.kind,
                                mouse.modifiers,
                                mouse.column,
                                mouse.row,
                            ) {
                                let _ = self.home.clear_preview_selection();
                                self.execute_action(action, terminal)?;
                                if let Some(session_id) =
                                    self.pending_structured_view_open.take()
                                {
                                    self.open_structured_view(&session_id).await?;
                                }
                                self.sync_mouse_capture(terminal)?;
                                if self.should_quit {
                                    break;
                                }
                                if !self.needs_redraw {
                                    self.draw(terminal)?;
                                }
                                continue;
                            }
                            // aoe captures the mouse, so the host terminal can't
                            // open links. Shift keeps aoe out of the way.
                            if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
                                && !mouse.modifiers.contains(KeyModifiers::SHIFT)
                            {
                                if let Some(url) =
                                    self.home.preview_link_at(mouse.column, mouse.row)
                                {
                                    // Over SSH no visible browser opens; OSC 52
                                    // still reaches the user's clipboard.
                                    let status = match crate::tui::open_url::open_url(&url) {
                                        Ok(()) => format!("opened {url}"),
                                        Err(e) => {
                                            crate::tui::clipboard::copy_to_clipboard(&url);
                                            format!("{e}; copied {url}")
                                        }
                                    };
                                    self.home.flash_status(status);
                                    let _ = self.home.clear_preview_selection();
                                    // Otherwise clicking the link again counts as a double-click.
                                    self.home.forget_preview_click();
                                    self.draw(terminal)?;
                                    continue;
                                }
                            }
                            // Mouse-tracking agents get presses and drags as if
                            // attached; Shift falls through to aoe's selection.
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
                            // Settings covers the stale list/preview rects.
                            let hit_scroll_target = hit_diff
                                || hit_list
                                || hit_preview
                                || self.home.is_settings_open();
                            // Left-click priority: context menu, dialog, sidebar
                            // toggle, diagnostics, tips badge, drag start, list
                            // row, diff file list.
                            let click_action = if matches!(
                                mouse.kind,
                                MouseEventKind::Down(MouseButton::Left)
                            ) {
                                if self
                                    .home
                                    .handle_context_menu_click(mouse.column, mouse.row)
                                {
                                    self.draw(terminal)?;
                                    None
                                } else if self.home.handle_dialog_click(mouse.column, mouse.row)
                                {
                                    let _ = self.home.clear_preview_selection();
                                    // Apply an intro theme pick before the next frame.
                                    if let Some(name) = self.home.take_pending_intro_theme() {
                                        self.set_theme(&name);
                                    }
                                    self.sync_mouse_capture(terminal)?;
                                    self.draw(terminal)?;
                                    None
                                } else if self
                                    .home
                                    .handle_sidebar_collapse_click(mouse.column, mouse.row)
                                    || self
                                        .home
                                        .handle_diagnostics_click(mouse.column, mouse.row)
                                    || self.home.handle_tips_badge_click(mouse.column, mouse.row)
                                {
                                    let _ = self.home.clear_preview_selection();
                                    self.draw(terminal)?;
                                    None
                                } else if self
                                    .home
                                    .handle_drag_start(mouse.column, mouse.row)
                                {
                                    // A divider drag drops an unrelated highlight.
                                    if !self.home.is_preview_select_dragging() {
                                        let _ = self.home.clear_preview_selection();
                                    }
                                    None
                                } else if hit_list {
                                    let _ = self.home.clear_preview_selection();
                                    let action = self
                                        .home
                                        .handle_click(mouse.column, mouse.row);
                                    // Empty list space opens the new-session dialog, like `n`.
                                    if action.is_none() {
                                        let _ = self
                                            .home
                                            .handle_empty_list_click(mouse.column, mouse.row);
                                    }
                                    self.draw(terminal)?;
                                    action
                                } else if hit_diff {
                                    let _ = self.home.clear_preview_selection();
                                    self.home.handle_diff_click(mouse.column, mouse.row);
                                    self.draw(terminal)?;
                                    None
                                } else if self.home.clear_preview_selection() {
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
                                MouseEventKind::Drag(MouseButton::Left) => {
                                    self.home.handle_drag_move(mouse.column, mouse.row)
                                }
                                MouseEventKind::Up(MouseButton::Left) => {
                                    // Clipboard write waits for the next draw, which
                                    // captures the cell text.
                                    self.home.handle_drag_end()
                                }
                                MouseEventKind::Down(MouseButton::Right) if hit_list => {
                                    self.home.handle_right_click(mouse.column, mouse.row)
                                }
                                // Unguarded so hover clears when leaving the list.
                                MouseEventKind::Moved => {
                                    // Agents with hover tracking see bare motion too.
                                    self.home
                                        .forward_hover_to_preview(mouse.column, mouse.row);
                                    let mut changed =
                                        self.home.handle_hover(mouse.column, mouse.row);
                                    changed |= self
                                        .home
                                        .update_hovered_link(mouse.column, mouse.row);
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
                            if let Some(text) = self.home.take_preview_copy_text() {
                                crate::tui::clipboard::copy_to_clipboard(&text);
                            }
                            if let Some(action) = click_action {
                                self.execute_action(action, terminal)?;
                                if let Some(session_id) = self.pending_structured_view_open.take() {
                                    self.open_structured_view(&session_id).await?;
                                }
                            }
                            // Actions stashed by dialog button clicks.
                            if let Some(action) = self.home.pending_dialog_click_action.take() {
                                self.execute_action(action, terminal)?;
                                if let Some(session_id) = self.pending_view_switch.take() {
                                    self.perform_view_switch(&session_id, terminal).await;
                                }
                                if let Some(session_id) = self.pending_daemon_start_open.take() {
                                    self.start_daemon_then_open(&session_id, terminal).await;
                                }
                                if let Some(session_id) = self.pending_smart_rename.take() {
                                    self.perform_smart_rename(&session_id).await;
                                }
                            }
                            continue;
                        }
                        Some(Ok(Event::Paste(text))) => {
                            crate::session::write_tui_activity();
                            // Only an ACTIVE structured view owns pasted text.
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
                            // Redraw so viewport-driven layout re-evaluates.
                            self.draw(terminal)?;
                            continue;
                        }
                        Some(Ok(_)) => {}
                        Some(Err(e)) => {
                            // The tty is gone.
                            tracing::info!(target: "tui.input", "Terminal event stream error, exiting: {}", e);
                            self.should_quit = true;
                            break;
                        }
                        None => {
                            tracing::info!(target: "tui.input", "Terminal event stream ended (EOF), exiting");
                            self.should_quit = true;
                            break;
                        }
                    }
                }
                // `next_event` is cancel-safe; the apply runs in the arm body.
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
                    woke_via_preview = true;
                }
                _ = async {
                    match post_key_deadline {
                        Some(at) => tokio::time::sleep_until(at.into()).await,
                        None => std::future::pending::<()>().await,
                    }
                } => {
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

            let mut refresh_needed = false;
            // Deterministic signals that bypass the live-send cool-down.
            let mut full = false;

            full |= self.home.expire_status_flash();
            // Diffed redraw, not `needs_redraw`: a clear per tick strobes.
            full |= self.home.tick_preview_autoscroll();
            refresh_needed |= self.home.tick_unread_dwell(std::time::Instant::now());

            // Banner changes shift the layout, so they need a full clear.
            let banner_changed = self.poll_update_check()
                | self.poll_update_status()
                | self.poll_image_update_check()
                | self.poll_image_pull_status();
            if banner_changed {
                self.needs_redraw = true;
                full = true;
            }

            if last_status_refresh.elapsed() >= STATUS_REFRESH_INTERVAL {
                self.home.request_status_refresh();
                self.home.repair_session_id_pollers();
                last_status_refresh = std::time::Instant::now();
            }
            full |= self.home.apply_status_updates();

            if last_metrics_sample.elapsed() >= METRICS_SAMPLE_INTERVAL {
                self.home.request_metrics_refresh();
                last_metrics_sample = std::time::Instant::now();
            }
            refresh_needed |= self.home.apply_metrics_updates();

            if last_session_feed_refresh.elapsed() >= SESSION_FEED_REFRESH_INTERVAL {
                self.home.request_session_feed_refresh();
                last_session_feed_refresh = std::time::Instant::now();
            }
            full |= self.home.apply_session_feed();
            full |= self.home.apply_structured_approval_results();
            full |= self.home.apply_deletion_results();
            full |= self.home.apply_stop_results();
            full |= self.home.apply_trash_results();
            full |= self.home.apply_reconcile_results();

            if last_session_idle_reap.elapsed() >= SESSION_IDLE_REAP_INTERVAL {
                last_session_idle_reap = std::time::Instant::now();
                full |= self.reap_idle_sessions();
            }

            full |= self.home.apply_session_id_updates();
            full |= self.home.apply_recovery_updates();
            full |= self.home.apply_restart_results();
            for session_id in self.home.take_restarted_attaches() {
                self.attach_live_session(&session_id, terminal)?;
                full = true;
            }
            full |= self.home.apply_attach_project_results();

            let store_move = self.home.poll_store_move();
            full |= store_move.changed;
            if let Some(action) = store_move.resume {
                self.execute_action(action, terminal)?;
                full = true;
            }

            if let Some(session_id) = self.home.apply_creation_results() {
                self.dispatch_new_session_attach(&session_id, terminal)?;
                if let Some(sid) = self.pending_structured_view_open.take() {
                    self.open_structured_view(&sid).await?;
                }
                full = true;
            }

            full |= self.home.tick_dialog();
            full |= self.home.tick_settings_status();

            // Full/config reloads stay deferred during live-send to preserve input
            // policy and mouse-capture state.
            let live_idle = self.home.live_send.is_none();
            if take_config_refresh_kick(live_idle, &self.home.config_watch.dirty) {
                let result = self.home.try_refresh_from_config_watcher();
                handle_tick_reload_config(result, &mut self.home.reload_failure_state);
                if let Some(theme_name) = self.home.take_pending_watcher_theme() {
                    self.set_theme(&theme_name);
                }
                full = true;
            }

            let heartbeat_due = last_disk_refresh.elapsed() >= DISK_REFRESH_INTERVAL;
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
                    full = true;
                }
                DiskRefreshDecision::StorageOnly => {
                    let reload_result = self.home.reload_storage_only();
                    handle_tick_reload_storage(reload_result, &mut self.home.reload_failure_state);
                    if heartbeat_due {
                        last_disk_refresh = std::time::Instant::now();
                    }
                    full = true;
                }
                DiskRefreshDecision::None => {}
            }

            full |= self.home.try_present_reload_failure_dialog();
            full |= self.home.try_clear_recovered_reload_dialog();
            // Another surface took the size-owner lock: leave live mode.
            full |= self.home.poll_live_send_takeover();

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
                full = true;
            }

            full |= self.reconcile_structured_preview().await;

            // Same cadence keeps the embedded composer caret blinking.
            if let Some(view) = self.home.structured_preview.as_mut() {
                let toast_changed = view.tick();
                if toast_changed || last_spinner_redraw.elapsed() >= SPINNER_REDRAW_INTERVAL {
                    last_spinner_redraw = std::time::Instant::now();
                    full = true;
                }
            }

            // Live-send refreshes on every tick; preview wakes carry changed content.
            let live = self.home.live_send.is_some();
            refresh_needed |= full || live || woke_via_post_key || woke_via_preview;

            let in_cooldown = last_refresh_at.is_some_and(|t| t.elapsed() < REFRESH_COOLDOWN);
            if live && in_cooldown && !full && !woke_via_post_key && !woke_via_preview {
                refresh_needed = false;
            }

            if refresh_needed {
                self.draw(terminal)?;
                last_refresh_at = Some(std::time::Instant::now());
            }

            if self.should_quit {
                break;
            }
        }

        self.home.apply_session_id_updates();
        // Persist the final restart snapshot instead of a stale `Starting` row.
        self.home.apply_restart_results();
        self.home.cleanup_pending_creation();

        if let Err(e) = self.home.save() {
            tracing::error!(target: "tui.input", "Failed to save on quit: {}", e);
        }

        // Bounded and deduped so a dead endpoint or an unchanged launch-then-quit costs nothing.
        if let Some(snapshot) = self.build_telemetry_snapshot() {
            let reported = snapshot.session_creates_since_last_snapshot;
            let outcome = crate::telemetry::flush_snapshot_if_changed(snapshot).await;
            clear_reported_session_creates(reported, outcome);
        }

        Ok(())
    }

    /// `None` unless telemetry is opted in. The TUI hosts no dashboard or
    /// server, so those signals are zeroed or empty.
    fn build_telemetry_snapshot(&self) -> Option<crate::telemetry::UsageSnapshot> {
        let instances: Vec<crate::session::Instance> = self.home.instances().cloned().collect();
        crate::telemetry::build_usage_snapshot(
            crate::telemetry::Surface::Tui,
            &instances,
            crate::telemetry::usage_signals::zeroed(),
            reported_session_creates(),
            None,
            None,
            &crate::telemetry::StructuredInteractionCounts::default(),
        )
    }

    /// Detached send; the create count is cleared only after a confirmed send.
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

    /// Callers gate on the check mode and on no check being in flight.
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

    /// True when a fresh, non-snoozed update just arrived.
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

        if self.dismissed_update_version.as_deref() == Some(info.latest_version.as_str()) {
            self.update_info = None;
            return false;
        }

        true
    }

    fn sandbox_in_use(&self) -> bool {
        if Config::load_or_warn().sandbox.enabled_by_default {
            return true;
        }
        self.home.instances().any(|i| i.is_sandboxed())
    }

    /// The image banner has the lowest priority and stays hidden while its own
    /// pull runs, so `u` can't re-arm into a no-op.
    fn image_banner_active(&self) -> bool {
        self.image_update.is_some()
            && self.update_info.is_none()
            && self.update_status.is_none()
            && self.image_pull_rx.is_none()
    }

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

    fn poll_image_update_check(&mut self) -> bool {
        let Some(mut rx) = self.image_update_rx.take() else {
            return false;
        };
        match rx.try_recv() {
            Ok(Ok(Some(update))) => {
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

    /// Only a writable tarball install can update unattended: Homebrew wants
    /// `brew upgrade` and sudo can't prompt without a TTY.
    fn maybe_kick_off_auto_install(&mut self, version: String) {
        use crate::update::install::{detect_install_method, perform_update, InstallMethod};

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
        self.pending_install_version = Some(version.clone());
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.update_status_rx = Some(rx);
        let handle = tokio::runtime::Handle::current();
        std::thread::spawn(move || {
            let result = handle.block_on(perform_update(&method, &version, None));
            let _ = tx.send(result);
        });
    }

    fn poll_update_status(&mut self) -> bool {
        let Some(mut rx) = self.update_status_rx.take() else {
            return false;
        };
        match rx.try_recv() {
            Ok(Ok(())) => {
                self.last_installed_version_in_session = self.pending_install_version.take();
                self.update_status = Some(UpdateStatus::persistent(
                    "update complete. Restart aoe to use the new version.".into(),
                ));
                true
            }
            Ok(Err(e)) => {
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

    /// Homebrew and sudo installs suspend the TUI so prompts can use the terminal.
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
            // `perform_update`'s future is !Send, so it runs on a thread
            // blocking on the existing runtime.
            self.set_status(format!("updating to v{version}…"));
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

    fn set_status(&mut self, text: impl Into<String>) {
        self.update_status = Some(UpdateStatus::transient(text.into()));
    }
}

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

/// `elapsed = None` means no check ran yet, so enabling checks fires immediately.
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

/// Storage-only refreshes stay eligible during live-send; full reloads need idle.
fn decide_disk_refresh(live_idle: bool, heartbeat_due: bool, dirty: bool) -> DiskRefreshDecision {
    if live_idle && heartbeat_due {
        DiskRefreshDecision::FullHeartbeat
    } else if heartbeat_due || dirty {
        DiskRefreshDecision::StorageOnly
    } else {
        DiskRefreshDecision::None
    }
}

/// Keep an overdue full heartbeat across storage-only resets so it runs once live-send exits.
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

/// Log and record reload errors; the loop keeps the previous in-memory state.
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

/// Like storage reloads, but also keeps a malformed config from resetting
/// safety settings like `confirm_before_quit` to defaults.
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

#[derive(Debug, PartialEq, Eq)]
enum QuitIntent {
    /// Ctrl+Q is reserved for leaving live-send and never quits.
    Ignore,
    ConfirmDuringCreation,
    Confirm,
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

    /// On-demand "Auto-name now"; the new title arrives through the file watcher.
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

    /// POST the view switch, starting a local daemon first if needed: the user
    /// just confirmed a dialog saying the agent restarts under `aoe serve`.
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

    /// Enter the structured view, mounting it first if needed, or offer to
    /// start a daemon when none runs.
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

    /// Live-send is exited first: both own the preview pane and keyboard.
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

    /// Mount in preview state; the caller activates it when entering.
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

    /// No `needs_redraw`: the diffed draw covers the pane, a full clear would flash.
    fn close_embedded_structured(&mut self) {
        self.home.structured_preview = None;
    }

    async fn start_daemon_then_open(
        &mut self,
        session_id: &str,
        terminal: &mut Terminal<TuiBackend>,
    ) {
        self.update_status = Some(UpdateStatus::transient("Starting local daemon…".into()));
        let _ = self.draw(terminal);
        match crate::tui::dialogs::start_local_daemon_and_wait().await {
            Ok(endpoint) => {
                self.update_status = None;
                self.connect_embedded_structured(endpoint, session_id).await;
                self.activate_embedded();
                self.drain_pending_paste_for_structured_view(session_id)
                    .await;
            }
            Err(e) => {
                let first = e.lines().next().unwrap_or("unknown error");
                self.set_status(format!("daemon start failed: {first}"));
            }
        }
    }

    /// Keep a streaming preview mounted for the selected structured session,
    /// debounced and only while a daemon is already reachable. Returns true if
    /// the mount set changed.
    async fn reconcile_structured_preview(&mut self) -> bool {
        // An active view stays unless a peer deleted, flipped, or archived its
        // session, or the selection moved: it would capture keys invisibly.
        if let Some(view) = self
            .home
            .structured_preview
            .as_ref()
            .filter(|v| v.is_active())
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
            .map(|v| v.session_id().to_string());
        if desired.as_deref() == mounted.as_deref() {
            self.clear_preview_mount_pending();
            return false;
        }
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
        self.home.structured_preview_pending = true;
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

    fn clear_preview_mount_pending(&mut self) {
        self.preview_mount_pending = None;
        self.home.structured_preview_pending = false;
    }

    /// Auto-stop plain tmux sessions idle past `session.auto_stop_idle_secs`.
    /// Claims happen under the storage lock so a co-running `aoe serve` can't
    /// double-stop. Returns true if any session was reaped.
    fn reap_idle_sessions(&mut self) -> bool {
        // Skip the pass on a tmux query failure rather than reap an attached session.
        let Ok(attached) = crate::tmux::attached_session_names() else {
            return false;
        };
        let now = chrono::Utc::now();
        let instances: Vec<crate::session::Instance> = self.home.instances().cloned().collect();
        let candidates = crate::session::idle_reap::idle_reap_candidates(
            &instances,
            now,
            &attached,
            |profile| {
                crate::session::config::profile_config::resolve_config_or_warn(profile)
                    .session
                    .auto_stop_idle_secs
            },
        );
        let mut reaped = false;
        for cand in candidates {
            match crate::session::idle_reap::claim_idle_stop(
                &cand.profile,
                self.home.file_watch.clone(),
                &cand.session_id,
                now,
                cand.threshold_secs,
            ) {
                Ok(Some(instance)) => {
                    // The claim persisted `Stopped`; kill off the UI thread.
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
        terminal: &mut Terminal<TuiBackend>,
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
                    // Background stop: `docker stop` can block for the grace
                    // period. Stopped now so the poller doesn't flag Error.
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
                self.home.execute_send_message(&id, &message);
                if !warm {
                    self.update_status = None;
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
                let outcome = self.home.prepare_live_send(&id);
                // Settle the toast first so the redraw computes the final geometry.
                if !warm {
                    // The info dialog already carries any failure detail.
                    self.update_status = None;
                }
                if outcome.is_ok() {
                    self.draw(terminal)?;
                }
            }
            Action::AttachToolSession(id, tool_name) => {
                self.attach_tool_session(&id, &tool_name, terminal)?;
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

    /// Route a new session through the configured attach mode. Structured
    /// sessions open their structured view instead.
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

        // Hook status or a custom command (wrapper scripts look like shells)
        // beats shell detection.
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
        if needs_restart {
            // Warn once when the agent can't take the sandbox's custom instruction.
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

            let skip_on_launch = self.home.take_on_launch_hooks_ran(session_id);
            // The attach follows from the tick loop; failures surface as dialogs.
            self.home
                .restart_then_attach(session_id, crate::terminal::get_size(), skip_on_launch);
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
        let tmux_session = match self.home.get_instance(session_id) {
            Some(inst) => inst.tmux_session()?,
            None => return Ok(()),
        };
        // Undo manual preview sizing so the attaching client gets the full
        // terminal, and re-assert preview geometry afterwards.
        tmux_session.reset_size_to_latest_client();
        self.home.clear_preview_pane_sync(session_id);
        let (attach_result, attached_status_updates) =
            self.with_attached_status_hooks(terminal, || tmux_session.attach())?;

        self.settle_after_attach(attached_status_updates)?;
        // Turns that finished during the attach were applied without the
        // live-send exemption; the user just viewed them.
        self.home.clear_unread_on_view(session_id);
        self.home.stamp_last_accessed(session_id);
        if let Err(e) = self.home.save() {
            tracing::error!("Failed to save after attach-return: {}", e);
        }
        self.select_after_attach(session_id);

        if let Err(e) = attach_result {
            tracing::warn!(target: "tui.input", "tmux attach returned error: {}", e);
        }

        Ok(())
    }

    fn settle_after_attach(&mut self, updates: Vec<StatusUpdate>) -> Result<()> {
        self.needs_redraw = true;
        crate::tmux::refresh_session_cache();
        self.home.reload()?;
        self.home.apply_status_updates_without_hooks(updates);
        Ok(())
    }

    /// In Attention sort, jump to the top-attention row: the session we left
    /// usually dropped a tier.
    fn select_after_attach(&mut self, session_id: &str) {
        if self.home.sort_order() == crate::session::config::SortOrder::Attention {
            self.home.select_top_attention(Some(session_id));
        } else {
            self.home.select_session_by_id(session_id);
        }
    }

    /// Start a pending sandbox store copy and resume `resume` afterwards,
    /// returning `true` when the launch was deferred.
    fn defer_to_store_move(&mut self, session_id: &str, resume: Action) -> bool {
        if !self.home.needs_store_move_before_launch(session_id) {
            return false;
        }
        if !self.home.begin_store_move(session_id, Some(resume)) {
            self.set_status("another agent store move is still in progress");
        }
        true
    }

    fn attach_terminal(
        &mut self,
        session_id: &str,
        mode: TerminalMode,
        terminal: &mut Terminal<TuiBackend>,
    ) -> Result<()> {
        let instance = match self.home.get_instance(session_id) {
            Some(inst) => inst.clone(),
            None => return Ok(()),
        };

        let size = crate::terminal::get_size();

        let attach_fn: Box<dyn FnOnce() -> Result<()>> = match mode {
            TerminalMode::Container if instance.is_sandboxed() => {
                let container_session = instance.container_terminal_tmux_session()?;
                if !container_session.exists() || container_session.is_pane_dead() {
                    if self.defer_to_store_move(
                        session_id,
                        Action::AttachTerminal(session_id.to_string(), mode),
                    ) {
                        return Ok(());
                    }
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

        self.settle_after_attach(attached_status_updates)?;
        self.select_after_attach(session_id);

        if let Err(e) = attach_result {
            tracing::warn!(target: "tui.input", "tmux terminal attach returned error: {}", e);
        }

        Ok(())
    }

    fn attach_tool_session(
        &mut self,
        session_id: &str,
        tool_name: &str,
        terminal: &mut Terminal<TuiBackend>,
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
            if let Err(e) = tool_session.create_with_size(
                &instance.project_path,
                &tool_config.command,
                size,
                &instance.effective_profile(),
            ) {
                self.home
                    .set_instance_error(session_id, Some(e.to_string()));
                return Ok(());
            }
            // A bad tool command can exit instantly and leave a dead pane.
            if let Err(e) = tool_session.wait_until_ready() {
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
            &instance.effective_profile(),
        );

        let attach_fn: Box<dyn FnOnce() -> Result<()>> = Box::new(move || tool_session.attach());
        let (attach_result, attached_status_updates) =
            self.with_attached_status_hooks(terminal, attach_fn)?;

        self.settle_after_attach(attached_status_updates)?;
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

    fn run_background_tool_session(&mut self, session_id: &str, tool_name: &str) {
        let Some(project_path) = self
            .home
            .get_instance(session_id)
            .map(|i| i.project_path.clone())
        else {
            self.set_status(format!("Tool '{tool_name}' failed: session not found"));
            return;
        };
        let Some(command) = self
            .home
            .tool_configs
            .get(tool_name)
            .map(|t| t.command.clone())
        else {
            self.set_status(format!("Tool '{tool_name}' is not configured"));
            return;
        };
        let status = match spawn_background_tool(session_id, tool_name, &project_path, &command) {
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
        let editor_available = |name: &str| {
            std::process::Command::new(name)
                .arg("--version")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok()
        };
        let editor = std::env::var("EDITOR")
            .ok()
            .or_else(|| {
                ["vim", "nano"]
                    .into_iter()
                    .find(|name| editor_available(name))
                    .map(str::to_string)
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

        if let Some(ref mut diff_view) = self.home.diff_view {
            if let Err(e) = diff_view.refresh_files() {
                tracing::warn!(target: "tui.input", "Failed to refresh diff after edit: {}", e);
            }
        }

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
    /// Deferred so the loop can show "pulling…" first.
    SpawnImagePull(String),
    /// Deferred so the loop can show "Reviving..." before `ensure_pane_ready`.
    SendMessage(String, String),
    EnterLiveSend(String),
    /// A session from the synchronous create path, routed through the new-session mode.
    AttachAfterCreate(String),
    /// (session id, tool name indexing `Config.tools`).
    AttachToolSession(String, String),
    /// Run a tool command detached in the session's workdir.
    RunBackgroundToolSession(String, String),
    // Stashed for the async loop.
    OpenStructuredView(String),
    SwitchSessionView(String),
    StartDaemonThenOpenStructured(String),
    SmartRenameNow(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::SendOutcome;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Read a signal's disposition; `sigaction` sets while reading, so restore it.
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
            (SigHandler::SigDfl, SigHandler::SigDfl) | (SigHandler::SigIgn, SigHandler::SigIgn) => {
                true
            }
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
        let mut app = App::new(
            "test",
            crate::tmux::AvailableTools::with_tools(&["claude"]),
            true,
            false,
            crate::file_watch::FileWatchService::noop(),
        )
        .expect("app");
        let pending = &mut app.home.pending_paste_for_structured_view;
        pending.insert("s-1".into(), "buffered draft".into());
        pending.insert("s-2".into(), "other target".into());
        app.home.structured_preview =
            Some(crate::tui::structured_view::embedded::EmbeddedView::for_test("s-1"));

        // A failed attempt to mount s-2 can leave s-1 mounted.
        app.drain_pending_paste_for_structured_view("s-2").await;
        let composer = |app: &App| {
            app.home
                .structured_preview
                .as_ref()
                .unwrap()
                .composer_text()
        };
        assert_eq!(composer(&app), "");
        assert_eq!(
            app.home
                .pending_paste_for_structured_view
                .get("s-1")
                .map(String::as_str),
            Some("buffered draft")
        );

        app.drain_pending_paste_for_structured_view("s-1").await;
        app.drain_pending_paste_for_structured_view("s-1").await;
        assert_eq!(composer(&app), "buffered draft");
        let pending = &app.home.pending_paste_for_structured_view;
        assert_eq!(pending.get("s-1"), None);
        assert_eq!(pending.get("s-2").map(String::as_str), Some("other target"));
    }

    #[test]
    #[cfg(unix)]
    #[serial_test::serial]
    fn ignore_signals_guard_installs_sig_ign_and_restores_prior_disposition() {
        use nix::sys::signal::{SigHandler, Signal};

        let signals = [Signal::SIGINT, Signal::SIGQUIT];
        let baseline = signals.map(current_disposition);
        let guard = IgnoreSignalsGuard::new();
        for signal in signals {
            assert!(same_disposition(
                current_disposition(signal),
                SigHandler::SigIgn
            ));
        }
        drop(guard);
        for (signal, before) in signals.into_iter().zip(baseline) {
            assert!(same_disposition(current_disposition(signal), before));
        }
    }

    // The counter is process-global, hence the serial group.
    #[test]
    #[serial_test::serial(telemetry_creates)]
    fn create_counter_clears_only_what_a_confirmed_send_reported() {
        let count = || TUI_SESSION_CREATES.load(Ordering::Relaxed);

        TUI_SESSION_CREATES.store(0, Ordering::Relaxed);
        for _ in 0..3 {
            record_session_create();
        }
        let reported = reported_session_creates();
        assert_eq!(reported, 3);
        record_session_create(); // lands while the send is in flight
        clear_reported_session_creates(reported, SendOutcome::Sent);
        assert_eq!(count(), 1);

        for outcome in [SendOutcome::Failed, SendOutcome::Deduped] {
            TUI_SESSION_CREATES.store(2, Ordering::Relaxed);
            clear_reported_session_creates(2, outcome);
            assert_eq!(count(), 2, "{outcome:?} must retain the count");
        }

        TUI_SESSION_CREATES.store(3, Ordering::Relaxed);
        clear_reported_session_creates(0, SendOutcome::Sent);
        assert_eq!(count(), 3);
        TUI_SESSION_CREATES.store(2, Ordering::Relaxed);
        clear_reported_session_creates(5, SendOutcome::Sent);
        assert_eq!(count(), 0, "saturates instead of wrapping");
    }

    #[test]
    fn quit_intent_policy() {
        use QuitIntent::*;
        let cases = [
            // Ctrl+Q never quits (#1569).
            (KeyModifiers::CONTROL, false, false, Ignore),
            (KeyModifiers::CONTROL, true, true, Ignore),
            (KeyModifiers::NONE, false, false, Quit),
            (KeyModifiers::NONE, false, true, Confirm),
            (KeyModifiers::NONE, true, false, ConfirmDuringCreation),
            (KeyModifiers::NONE, true, true, ConfirmDuringCreation),
        ];
        for (mods, creating, confirm, expected) in cases {
            assert_eq!(quit_intent(mods, creating, confirm), expected);
        }
    }

    #[test]
    fn disk_refresh_decisions() {
        use DiskRefreshDecision::*;
        // (live_idle, heartbeat_due, dirty) -> decision
        let cases = [
            (true, true, true, FullHeartbeat),
            (true, true, false, FullHeartbeat),
            (true, false, true, StorageOnly),
            (true, false, false, None),
            (false, false, false, None),
            (false, true, false, StorageOnly),
            (false, false, true, StorageOnly),
            (false, true, true, StorageOnly),
        ];
        for (idle, heartbeat, dirty, expected) in cases {
            assert_eq!(
                decide_disk_refresh(idle, heartbeat, dirty),
                expected,
                "idle={idle} heartbeat={heartbeat} dirty={dirty}"
            );
        }

        let live_plan = plan_disk_refresh(false, true, false, false);
        assert_eq!(live_plan.decision, StorageOnly);
        assert!(live_plan.full_heartbeat_deferred);
        let idle_plan = plan_disk_refresh(true, false, false, true);
        assert_eq!(idle_plan.decision, FullHeartbeat);
        assert!(!idle_plan.full_heartbeat_deferred);
    }

    #[test]
    fn config_refresh_kick_is_gated_by_live_send() {
        let dirty = AtomicBool::new(true);
        assert!(!take_config_refresh_kick(false, &dirty));
        assert!(
            dirty.load(Ordering::Acquire),
            "stays latched for the next idle tick"
        );
        assert!(take_config_refresh_kick(true, &dirty));
        assert!(!dirty.load(Ordering::Acquire));
    }

    #[test]
    fn poll_update_receiver_states() {
        let info = |available, latest: &str| UpdateInfo {
            available,
            current_version: "0.4.0".to_string(),
            latest_version: latest.to_string(),
        };

        let (tx, rx) = tokio::sync::oneshot::channel();
        tx.send(Ok(info(true, "0.5.0"))).unwrap();
        let (got, rx_out, received) = poll_update_receiver(Some(rx), None);
        assert!(received && rx_out.is_none());
        assert_eq!(got.unwrap().latest_version, "0.5.0");

        let (tx, rx) = tokio::sync::oneshot::channel();
        tx.send(Ok(info(false, "0.4.0"))).unwrap();
        let (got, rx_out, received) = poll_update_receiver(Some(rx), None);
        assert!(!received && got.is_none() && rx_out.is_none());

        let (_tx, rx) = tokio::sync::oneshot::channel::<anyhow::Result<UpdateInfo>>();
        let (got, rx_out, received) = poll_update_receiver(Some(rx), None);
        assert!(
            !received && got.is_none() && rx_out.is_some(),
            "empty keeps the receiver"
        );

        let (got, _, received) = poll_update_receiver(None, Some(info(true, "0.5.0")));
        assert!(!received);
        assert_eq!(
            got.unwrap().latest_version,
            "0.5.0",
            "existing info is kept"
        );
    }

    #[test]
    fn periodic_recheck_policy() {
        let interval = Duration::from_secs(3600);
        let second = Duration::from_secs(1);
        // (elapsed, in_flight, enabled) -> spawn
        let cases = [
            (Some(interval + second), false, true, true),
            (Some(interval), false, true, true),
            (Some(interval - second), false, true, false),
            (Some(interval + second), true, true, false),
            (Some(interval + second), false, false, false),
            (None, false, true, true),
            (None, false, false, false),
        ];
        for (elapsed, in_flight, enabled, expected) in cases {
            assert_eq!(
                should_spawn_periodic_update_check(elapsed, interval, in_flight, enabled),
                expected,
                "elapsed={elapsed:?} in_flight={in_flight} enabled={enabled}"
            );
        }
    }

    fn keys(text: &str) -> Vec<KeyEvent> {
        text.chars()
            .map(|c| match c {
                '\n' => KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                c => KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
            })
            .collect()
    }

    #[test]
    fn burst_candidates_and_chars() {
        let key = KeyEvent::new;
        for k in keys("a ~\n")
            .into_iter()
            .chain([key(KeyCode::Char('A'), KeyModifiers::SHIFT)])
        {
            assert!(App::is_burst_candidate(&k), "{k:?}");
            assert!(App::burst_char_for(&k).is_some(), "{k:?}");
        }
        assert_eq!(App::burst_char_for(&keys("\n")[0]), Some('\n'));
        for k in [
            key(KeyCode::Char('c'), KeyModifiers::CONTROL),
            key(KeyCode::Char('b'), KeyModifiers::ALT),
            key(KeyCode::Esc, KeyModifiers::NONE),
            key(KeyCode::Tab, KeyModifiers::NONE),
            key(KeyCode::Up, KeyModifiers::NONE),
            key(KeyCode::Backspace, KeyModifiers::NONE),
        ] {
            assert!(!App::is_burst_candidate(&k), "{k:?}");
        }
    }

    #[test]
    fn auto_repeat_burst_rejects_held_navigation_but_not_pasted_text() {
        let j = |kind| KeyEvent::new_with_kind(KeyCode::Char('j'), KeyModifiers::NONE, kind);
        let cases = [
            (keys("jjj"), true),
            (keys("kkk"), true),
            (
                vec![
                    j(KeyEventKind::Press),
                    j(KeyEventKind::Repeat),
                    j(KeyEventKind::Repeat),
                ],
                true,
            ),
            (keys("paste"), false),
        ];
        for (keys, expected) in cases {
            assert_eq!(App::is_auto_repeat_burst(&keys), expected, "{keys:?}");
        }
    }

    #[test]
    fn split_trailing_enter_peels_only_the_last_enter() {
        // (burst, paste text, trailing enter)
        let cases = [
            ("hi\n", "hi", true),
            ("a\nb\n", "a\nb", true),
            ("hi\nthere", "hi\nthere", false),
            ("abc", "abc", false),
            ("\n", "", true),
            ("hi\n\n", "hi\n", true),
        ];
        for (burst, paste, enter) in cases {
            let (got, trailing) = App::split_trailing_enter(burst, &keys(burst));
            assert_eq!(got, paste, "{burst:?}");
            assert_eq!(
                trailing.map(|k| k.code),
                enter.then_some(KeyCode::Enter),
                "{burst:?}"
            );
        }
    }
}
