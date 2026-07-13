//! Terminal User Interface module

mod app;
mod attached_status_hooks;
pub(crate) mod clipboard;
mod components;
mod creation_poller;
mod deletion_poller;
pub mod dialogs;
pub mod diff;
pub(crate) mod home;
#[cfg(feature = "serve")]
pub(crate) mod plugin_ui;
#[cfg(feature = "serve")]
pub(crate) mod remote_home;
pub(crate) mod responsive;
mod restart_poller;
pub mod settings;
mod status_poller;
mod stop_poller;
#[cfg(feature = "serve")]
pub(crate) mod structured_view;
pub(crate) mod styles;
mod worker;

pub use app::*;

/// Entry point for the hidden `aoe __vt-pipe <socket>` helper subprocess used
/// by the `AOE_VT_LIVE` live-preview path (default on). Copies the pane's piped
/// output (stdin) to the unix socket, unbuffered. Dispatched in `main` before
/// clap so it stays off the CLI/docs surface.
#[cfg(unix)]
pub fn run_vt_pipe(socket: &str) -> std::io::Result<()> {
    crate::tmux::vt::run_pipe(socket)
}

#[cfg(not(unix))]
pub fn run_vt_pipe(_socket: &str) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "__vt-pipe is unix-only",
    ))
}

use anyhow::Result;
use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::prelude::*;
use std::io::{self, IsTerminal, Write};

use crate::migrations;

/// Whether the TUI should request mouse capture (`\e[?1000h` etc.) from the
/// terminal. The Settings entry (Interaction > Mouse Capture, backed by
/// `session.mouse_capture`) is the primary control; the `AOE_MOUSE_CAPTURE`
/// env var stays as an opt-out backstop for environments where the toggle
/// isn't reachable (e.g. iOS Mosh + Termius/Blink, which don't reliably
/// forward mouse-tracking escapes to mobile clients). Capture is requested
/// only when the config allows it AND the env var hasn't disabled it, so a
/// `false` from either source wins and an existing `AOE_MOUSE_CAPTURE=0`
/// keeps working. Default ON to preserve the preview-pane mouse-wheel scroll
/// feature added in #795.
pub fn mouse_capture_requested(session: &crate::session::config::SessionConfig) -> bool {
    session.mouse_capture && env_mouse_capture_allows()
}

/// The legacy `AOE_MOUSE_CAPTURE` opt-out: `0`/`false` disables capture, any
/// other value (or an unset var) leaves it enabled. Kept as a backstop to
/// the Settings toggle.
fn env_mouse_capture_allows() -> bool {
    std::env::var("AOE_MOUSE_CAPTURE")
        .map(|v| !(v == "0" || v.eq_ignore_ascii_case("false")))
        .unwrap_or(true)
}

/// RAII guard for the TUI's terminal mode. `enter` turns on raw mode, the
/// alternate screen, bracketed paste, the kitty keyboard protocol
/// `DISAMBIGUATE_ESCAPE_CODES` flag (so `Shift+Enter` arrives as a distinct
/// `KeyEvent` instead of collapsing to bare CR, #2362), and (when requested)
/// mouse capture; the `Drop` impl reverses all of it. Because it runs on drop,
/// the terminal is restored on EVERY exit path, including a panic mid-render,
/// where the old inline teardown was skipped and left the terminal wedged (raw
/// mode / mouse reporting / enhancement stack stuck on). Drop is best-effort
/// and never panics.
///
/// The kitty-enhancement pop depends on Rust's default `panic = "unwind"`. A future
/// profile that sets `panic = "abort"` would skip every Drop here and leak
/// raw mode, alt screen, paste, mouse, and the enhancement stack into the
/// user's shell; recovery from a leaked enhancement stack is
/// `printf '\e[<1u'`. Same exposure as a SIGKILL/SIGSEGV mid-TUI.
struct TerminalGuard {
    /// Whether to emit `DisableMouseCapture` on teardown. Mirrors the startup
    /// gate: under Mosh we never enable capture, so we must not disable it
    /// either; otherwise we always disable (a mid-session settings toggle can
    /// turn it on after startup, so we can't gate on the startup value).
    disable_mouse: bool,
}

impl TerminalGuard {
    fn enter(enable_mouse: bool, mosh_active: bool) -> Result<Self> {
        // Roll back any already-applied state if a later step fails, so a
        // partial enter (e.g. raw mode on, alternate screen failed) never
        // leaves the shell wedged before a guard exists to restore it on drop.
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(err) = execute!(stdout, EnterAlternateScreen, EnableBracketedPaste) {
            let _ = disable_raw_mode();
            return Err(err.into());
        }
        if enable_mouse {
            if let Err(err) = execute!(stdout, EnableMouseCapture) {
                let _ = execute!(stdout, LeaveAlternateScreen, DisableBracketedPaste);
                let _ = disable_raw_mode();
                return Err(err.into());
            }
        }
        // Push the kitty keyboard protocol's DISAMBIGUATE_ESCAPE_CODES flag so
        // crossterm's parser sees `Shift+Enter` as `KeyEvent { Enter, SHIFT }`
        // instead of a bare CR indistinguishable from plain Enter (#2362). On
        // every kitty-protocol-capable terminal (Ghostty, Kitty, WezTerm,
        // foot, Konsole 24+, recent Alacritty/xterm) this enables the
        // `translate()` Shift+Enter arm in live_send. Non-supporting terminals
        // (Apple Terminal, default iTerm2, Termius, Mosh) silently ignore the
        // unknown `ESC[>1u` CSI; the user falls back to today's behavior.
        //
        // No `supports_keyboard_enhancement()` probe: it blocks for up to 2s
        // on unresponsive terminals (slow SSH, mosh) and conflicts with the
        // concurrent EventStream reader the TUI is about to start. Unknown
        // CSI is a safer default than a 2s startup stall.
        //
        // Only `DISAMBIGUATE_ESCAPE_CODES`. NOT `REPORT_EVENT_TYPES` (would
        // start emitting `KeyEventKind::Release` events that several input
        // pumps would need explicit filtering for). NOT `REPORT_ALTERNATE_KEYS`
        // (broader change in `KeyEvent` shape that would re-test every chord).
        //
        // Best-effort: a push failure here means we lose the Shift+Enter
        // distinction (status quo before #2362), not anything worth aborting
        // TUI startup for. Mirrors the Drop pop's best-effort posture.
        #[cfg(unix)]
        if let Err(err) = execute!(
            stdout,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES),
        ) {
            tracing::debug!(
                target: "tui.input",
                "kitty keyboard enhancement push failed (Shift+Enter will submit instead of inserting newline): {err}",
            );
        }
        Ok(Self {
            disable_mouse: !mosh_active,
        })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
        // Pop the kitty enhancement stack first, before anything else, so the
        // shell never inherits an active stack even if a later restore fails.
        // Best-effort, ignore errors.
        #[cfg(unix)]
        let _ = execute!(stdout, PopKeyboardEnhancementFlags);
        let _ = disable_raw_mode();
        if self.disable_mouse {
            let _ = execute!(stdout, DisableMouseCapture);
        }
        let _ = execute!(
            stdout,
            LeaveAlternateScreen,
            DisableBracketedPaste,
            crossterm::cursor::Show
        );
    }
}
use crate::session::get_update_settings;
use crate::update::check_for_update;

/// Clear the screen and force a full redraw on the next frame, without the
/// `ESC[6n` cursor-position round-trip that ratatui's `Terminal::clear` does.
///
/// ratatui 0.30's `Terminal::clear` snapshots the cursor via
/// `get_cursor_position` before clearing. That read shares crossterm's internal
/// event reader with our live `EventStream`; around stream lifecycle changes the
/// reader's poll wakes early and the cursor read completes with no matching
/// event, which surfaces as "cursor position could not be read within a normal
/// duration" (an immediate 0 ms failure, not the 2 s timeout the message
/// implies). Clearing the backend directly via `clear_region(ClearType::All)`
/// (the same call ratatui's Fullscreen `clear_viewport` issues) and resetting
/// the diff baseline repaints every cell on the next `draw` with no cursor
/// query.
///
/// A free function rather than an `App` method so both the local home view and
/// the remote home view route through one definition, keeping the "no clear
/// path calls `get_cursor_position`" invariant true across the whole TUI.
/// `ClearType::All` is correct for the `Viewport::Fullscreen` the TUI uses; an
/// inline or fixed viewport would need a different clear.
pub(crate) fn clear_terminal<B: Backend>(terminal: &mut Terminal<B>) -> Result<(), B::Error> {
    terminal
        .backend_mut()
        .clear_region(ratatui::backend::ClearType::All)?;
    // Reset both buffers so the next draw diffs against an empty baseline and
    // repaints the whole viewport.
    terminal.current_buffer_mut().reset();
    terminal.swap_buffers();
    Ok(())
}

pub async fn run(profile: &str, startup_warning: Option<String>) -> Result<()> {
    // Cross-machine entrypoint: when `AOE_DAEMON_URL` is set, swap the
    // local home view for the remote structured view picker so the user never
    // sees a session list that doesn't reflect the daemon they pointed
    // us at. Tmux check + migrations are intentionally skipped here:
    // the remote machine owns those, this side is a pure client.
    #[cfg(feature = "serve")]
    if let Some(endpoint) = crate::acp::client::discovery::discover_env() {
        let _ = startup_warning; // remote mode skips the local startup-warning channel
        let _ = profile;
        return remote_home::run_standalone(endpoint).await;
    }

    // Run pending migrations with a spinner so users see progress
    if migrations::has_pending_migrations() {
        const SPINNER_FRAMES: &[char] = &['◐', '◓', '◑', '◒'];
        let migration_handle = tokio::task::spawn_blocking(migrations::run_migrations);
        tokio::pin!(migration_handle);
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(120));
        let mut frame = 0usize;
        loop {
            tokio::select! {
                result = &mut migration_handle => {
                    print!("\r\x1b[2K");
                    let _ = io::stdout().flush();
                    result??;
                    break;
                }
                _ = tick.tick() => {
                    print!("\r  {} Running data migrations...", SPINNER_FRAMES[frame % SPINNER_FRAMES.len()]);
                    let _ = io::stdout().flush();
                    frame += 1;
                }
            }
        }
    }

    // Check for tmux
    if !crate::tmux::is_tmux_available() {
        eprintln!("Error: tmux not found in PATH");
        eprintln!();
        eprintln!("Agent of Empires requires tmux. Install with:");
        eprintln!("  brew install tmux     # macOS");
        eprintln!("  apt install tmux      # Debian/Ubuntu");
        eprintln!("  pacman -S tmux        # Arch");
        std::process::exit(1);
    }

    // Check for coding tools (no-agents case is handled inside the TUI)
    let available_tools = crate::tmux::AvailableTools::detect();

    // If version changed, refresh the update cache before showing TUI.
    // This ensures we have release notes for the changelog dialog.
    if check_version_change()?.is_some() {
        let settings = get_update_settings();
        if settings.update_check_mode.is_enabled() {
            let current_version = env!("CARGO_PKG_VERSION");
            // Don't let a network issue block startup
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                check_for_update(current_version, true),
            )
            .await;
        }
    }

    // Opt-in clean-only plugin auto-update sweep (off by default). Spawned
    // non-blocking so a slow remote or git never delays the TUI; applied updates
    // take effect on the next launch.
    // No notifier in the TUI: there is no plugin host / notification ring here.
    crate::plugin::auto_update::spawn_if_enabled(&crate::session::Config::load_or_warn(), None);

    // Bail early if stdin is not a terminal. Running without a tty would
    // cause the event loop to busy-loop after the parent terminal dies.
    if !io::stdin().is_terminal() {
        anyhow::bail!("stdin is not a terminal; aoe requires an interactive TTY");
    }

    // Setup terminal. Resolve the mouse/mosh policy BEFORE entering raw mode so
    // the RAII `TerminalGuard` owns the whole enter/restore lifecycle.
    //
    // Mouse capture is ON by default to preserve preview-pane wheel scroll
    // (#795); toggle it off via Settings > Interaction > Mouse Capture, or set
    // AOE_MOUSE_CAPTURE=0 as a backstop on iOS Mosh + Termius/Blink, which
    // can't reliably forward mouse-tracking escapes to mobile clients.
    //
    // Additionally: even when explicitly requested, Mosh mangles xterm
    // mouse-tracking escapes (inverted/duplicated scroll on Termius, Blink,
    // Mosh4iOS; broken right-click selection on desktop Mosh). MOSH_CONNECTION
    // is set by mosh-server and propagates through the user's environment;
    // when present, fall back to the terminal's native scroll regardless of
    // AOE_MOUSE_CAPTURE so the user can select text without aoe eating events.
    let mosh_active = std::env::var_os("MOSH_CONNECTION").is_some();
    // Resolve once for the startup enable; `App` re-resolves on its own reload
    // cadence so a mid-session settings toggle still applies.
    let startup_session_config = crate::session::resolve_config(profile)
        .map(|c| c.session)
        .unwrap_or_default();
    let enable_mouse = mouse_capture_requested(&startup_session_config) && !mosh_active;
    let _terminal_guard = TerminalGuard::enter(enable_mouse, mosh_active)?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    // Combine the caller-supplied startup warning (e.g. debug-log file
    // failures) with any config-parse failures we detect at startup.
    // `tracing::warn!` events from the `_or_warn` config helpers are dropped
    // by default in TUI mode (no subscriber attached), so we surface them
    // through the same InfoDialog channel here.
    //
    // Detected before `App::new` so we can suppress the first-run welcome /
    // changelog dialogs when there's a warning, both for UX (the warning is
    // the more important thing for the user to see) and to avoid overwriting
    // a malformed config.toml with defaults via `save_config`.
    let combined_warning = match (
        startup_warning,
        crate::session::collect_startup_config_warnings(profile),
    ) {
        (Some(a), Some(b)) => Some(format!("{a}\n\n{b}")),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    };

    // The TUI process owns its own FileWatchService Arc; threaded into every
    // consumer (HomeView, DiffView, per-profile Storage) so peer-process
    // writes to `sessions.json` / `groups.json` propagate within the
    // primitive's debounce window. Init failure must not abort the TUI;
    // fall back to a noop service. The 5s heartbeat path is the sole
    // reload signal in that case.
    let file_watch = crate::file_watch::FileWatchService::new().unwrap_or_else(|e| {
        tracing::warn!(
            target: "tui.file_watch",
            error = %e,
            "FileWatchService::new failed; live propagation disabled, falling back to 5s heartbeat"
        );
        crate::file_watch::FileWatchService::noop()
    });

    // Create app and run
    let mut app = App::new(
        profile,
        available_tools,
        combined_warning.is_some(),
        mosh_active,
        file_watch,
    )?;
    if let Some(warning) = combined_warning {
        app.show_startup_warning(&warning);
    }
    let result = app.run(&mut terminal).await;

    crate::session::clear_tui_heartbeat();

    // Terminal restore (raw mode, alternate screen, bracketed paste, mouse
    // capture, cursor) happens in `_terminal_guard`'s Drop, so it runs on every
    // exit path including a panic, not just this normal return.
    drop(terminal);
    result
}

#[cfg(test)]
mod clear_terminal_tests {
    use super::clear_terminal;
    use ratatui::{backend::TestBackend, buffer::Cell, widgets::Paragraph, Terminal};

    fn all_blank(terminal: &Terminal<TestBackend>) -> bool {
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .all(|c| c == &Cell::default())
    }

    /// `clear_terminal` must wipe the backend AND reset ratatui's diff baseline,
    /// so a clear followed by an identical redraw repaints every cell instead of
    /// diffing to a no-op and leaving a blank screen.
    #[test]
    fn clears_backend_and_repaints_on_next_identical_draw() {
        let mut terminal = Terminal::new(TestBackend::new(20, 3)).unwrap();

        terminal
            .draw(|f| f.render_widget(Paragraph::new("HELLO"), f.area()))
            .unwrap();
        assert!(!all_blank(&terminal), "draw should paint the backend");

        clear_terminal(&mut terminal).unwrap();
        assert!(
            all_blank(&terminal),
            "clear_terminal should wipe the backend"
        );

        // Same content again: without the diff-baseline reset the diff would be
        // empty and the screen would stay blank after the clear.
        terminal
            .draw(|f| f.render_widget(Paragraph::new("HELLO"), f.area()))
            .unwrap();
        assert!(!all_blank(&terminal), "redraw after clear must repaint");
    }
}

#[cfg(test)]
mod mouse_capture_tests {
    use super::mouse_capture_requested;
    use crate::session::config::SessionConfig;
    use serial_test::serial;

    /// Restores `AOE_MOUSE_CAPTURE` to its prior value on drop so the
    /// process-global env var doesn't leak between serial tests.
    struct EnvGuard(Option<String>);

    impl EnvGuard {
        fn set(val: Option<&str>) -> Self {
            let prev = std::env::var("AOE_MOUSE_CAPTURE").ok();
            apply(val);
            EnvGuard(prev)
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            apply(self.0.as_deref());
        }
    }

    fn apply(val: Option<&str>) {
        match val {
            Some(v) => std::env::set_var("AOE_MOUSE_CAPTURE", v),
            None => std::env::remove_var("AOE_MOUSE_CAPTURE"),
        }
    }

    fn session_with(mouse_capture: bool) -> SessionConfig {
        SessionConfig {
            mouse_capture,
            ..SessionConfig::default()
        }
    }

    #[test]
    #[serial]
    fn enabled_config_without_env_requests_capture() {
        let _g = EnvGuard::set(None);
        assert!(mouse_capture_requested(&session_with(true)));
    }

    #[test]
    #[serial]
    fn disabled_config_opts_out_even_without_env() {
        let _g = EnvGuard::set(None);
        assert!(!mouse_capture_requested(&session_with(false)));
    }

    #[test]
    #[serial]
    fn env_zero_still_wins_over_enabled_config() {
        // The pre-existing AOE_MOUSE_CAPTURE=0 escape hatch keeps working
        // even though the config defaults to enabled (#1346).
        let _g = EnvGuard::set(Some("0"));
        assert!(!mouse_capture_requested(&session_with(true)));
    }

    #[test]
    #[serial]
    fn env_true_does_not_re_enable_disabled_config() {
        let _g = EnvGuard::set(Some("1"));
        assert!(!mouse_capture_requested(&session_with(false)));
    }
}
