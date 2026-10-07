//! Terminal user interface.

mod app;
mod approval_poller;
mod attach_project_poller;
mod attached_status_hooks;
mod boot_spinner;
pub(crate) mod clipboard;
mod components;
mod creation_poller;
mod deletion_poller;
pub mod dialogs;
pub mod diff;
pub(crate) mod home;
mod host_title;
pub mod hyperlink;
pub(crate) mod links;
pub(crate) mod markdown;
mod metrics_poller;
pub(crate) mod open_url;
pub(crate) mod plugin_ui;
mod reconcile_poller;
pub(crate) mod remote_home;
pub(crate) mod responsive;
mod restart_poller;
mod session_feed;
pub mod settings;
mod status_poller;
mod stop_poller;
mod store_move_poller;
pub(crate) mod structured_view;
pub(crate) mod styles;
mod trash_poller;
mod worker;

pub use app::*;

/// Re-enter one libtest case in a private, killable process. Output stays off
/// the terminal, including OSC52 sequences emitted by clipboard tests.
#[cfg(test)]
fn isolated_test_process(test: &str, timeout: std::time::Duration) -> bool {
    use std::process::{Command, Stdio};
    let _env = crate::session::test_support::EnvGuard::read_lock();
    const CHILD: &str = "AOE_TUI_TEST_CHILD";
    const ENTERED: &str = "AOE_TUI_TEST_ENTERED";
    if std::env::var(CHILD).as_deref() == Ok(test) {
        std::fs::write(std::env::var_os(ENTERED).expect("child entry path"), test)
            .expect("acknowledge selected test");
        return true;
    }
    let home = tempfile::tempdir().expect("private child home");
    let socket = home.path().join("tmux.sock");
    let entered = home.path().join("entered");
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .env(CHILD, test)
        .env(ENTERED, &entered)
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path())
        .env("XDG_DATA_HOME", home.path())
        .env("XDG_CACHE_HOME", home.path())
        .env("TMPDIR", home.path())
        .env("AOE_TMUX_SOCKET", &socket)
        .env_remove("TMUX")
        .stdin(Stdio::null());
    let result = crate::process::run_with_timeout_process_group(&mut command, timeout);
    // tmux daemonizes out of the child's process group; reap its private
    // server after both normal exit and watchdog termination.
    if socket.exists() {
        let mut cleanup = Command::new("tmux");
        cleanup
            .arg("-S")
            .arg(&socket)
            .arg("kill-server")
            .stdin(Stdio::null());
        let cleanup = crate::process::run_with_timeout(&mut cleanup, timeout)
            .expect("kill private tmux server")
            .expect("private tmux cleanup timed out");
        assert!(cleanup.status.success(), "private tmux cleanup failed");
    }
    let output = result
        .expect("spawn isolated test")
        .expect("isolated test watchdog expired");
    assert!(
        output.status.success(),
        "isolated test failed: {:?}\n{:?}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(entered).expect("selected test did not run"),
        test
    );
    false
}

/// Hidden `aoe __vt-pipe <socket>` helper: copies the pane's piped output to the socket.
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
    terminal::{
        disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen, SetTitle,
    },
};
use ratatui::prelude::*;
use std::io::{self, IsTerminal};

use crate::migrations;
use crate::session::get_update_settings;
use crate::update::check_for_update;

/// Mouse capture needs both the `session.mouse_capture` setting and no
/// `AOE_MOUSE_CAPTURE=0|false` opt-out.
pub fn mouse_capture_requested(session: &crate::session::config::SessionConfig) -> bool {
    session.mouse_capture
        && std::env::var("AOE_MOUSE_CAPTURE")
            .map(|v| !(v == "0" || v.eq_ignore_ascii_case("false")))
            .unwrap_or(true)
}

/// Restores the terminal on every exit path, including a panic mid-render.
/// Under `panic = "abort"` nothing would be restored (recover the kitty stack
/// with `printf '\e[<1u'`).
struct TerminalGuard {
    /// False under Mosh, where capture is never enabled. Otherwise always
    /// disabled, since a settings toggle can enable it mid-session.
    disable_mouse: bool,
}

impl TerminalGuard {
    fn enter(enable_mouse: bool, mosh_active: bool) -> Result<Self> {
        // Roll back partial state so a failed enter never wedges the shell.
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
        // Kitty DISAMBIGUATE_ESCAPE_CODES makes Shift+Enter distinct from Enter;
        // other terminals ignore it. No support probe: it can stall startup for
        // 2s. Other flags would change key event shapes. Best-effort.
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
        // Pop the kitty stack first so the shell never inherits it.
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
            crossterm::cursor::Show,
        );
        if host_title::take_emitted() {
            let _ = execute!(stdout, SetTitle(host_title::FALLBACK_TITLE));
        }
    }
}

/// Clear the screen and force a full repaint without ratatui's `Terminal::clear`,
/// whose cursor-position query races the live `EventStream` and fails.
pub(crate) fn clear_terminal<B: Backend>(terminal: &mut Terminal<B>) -> Result<(), B::Error> {
    terminal
        .backend_mut()
        .clear_region(ratatui::backend::ClearType::All)?;
    terminal.current_buffer_mut().reset();
    terminal.swap_buffers();
    Ok(())
}

pub async fn run(profile: &str, startup_warning: Option<String>) -> Result<()> {
    // `AOE_DAEMON_URL` makes this a pure client of a remote daemon.
    if let Some(endpoint) = crate::acp::client::discovery::discover_env() {
        return remote_home::run_standalone(endpoint).await;
    }

    // Opening the store creates the profile directory, so refuse unknown names first.
    crate::session::require_known_profile(profile)?;

    // The spinner writes nothing unless a migration reports progress.
    {
        let console = std::sync::Arc::new(std::sync::Mutex::new(
            migrations::progress::ConsoleProgress::default(),
        ));
        let reporter: migrations::progress::Reporter = {
            let console = console.clone();
            std::sync::Arc::new(move |event| {
                console
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .apply(event);
            })
        };
        let migration_handle =
            tokio::task::spawn_blocking(move || migrations::run_migrations_with(Some(reporter)));
        tokio::pin!(migration_handle);
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(120));
        let mut frame = 0usize;
        let mut spinner = boot_spinner::BootSpinner::default();
        loop {
            tokio::select! {
                result = &mut migration_handle => {
                    let mut console = console.lock().unwrap_or_else(|e| e.into_inner());
                    let _ = spinner.finish(&mut io::stdout(), &mut console);
                    drop(console);
                    result??;
                    break;
                }
                _ = tick.tick() => {
                    let width = crate::terminal::get_size().map_or(80, |(w, _)| w as usize);
                    let mut console = console.lock().unwrap_or_else(|e| e.into_inner());
                    let _ = spinner.draw(&mut io::stdout(), &mut console, frame, width);
                    frame += 1;
                }
            }
        }
    }

    if !crate::tmux::is_tmux_available() {
        eprintln!("Error: tmux not found in PATH");
        eprintln!();
        eprintln!("Agent of Empires requires tmux. Install with:");
        eprintln!("  brew install tmux     # macOS");
        eprintln!("  apt install tmux      # Debian/Ubuntu");
        eprintln!("  pacman -S tmux        # Arch");
        std::process::exit(1);
    }

    let available_tools = crate::tmux::AvailableTools::detect();

    // Refresh the update cache so the changelog dialog has release notes.
    if check_version_change()?.is_some() && get_update_settings().update_check_mode.is_enabled() {
        // Don't let a network issue block startup.
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            check_for_update(env!("CARGO_PKG_VERSION"), true),
        )
        .await;
    }

    // Clean-only plugin auto-update, non-blocking; asks a running daemon to reload.
    crate::plugin::auto_update::spawn_if_enabled(&crate::session::Config::load_or_warn(), None);

    // Without a tty the event loop would busy-loop after the parent terminal dies.
    if !io::stdin().is_terminal() {
        anyhow::bail!("stdin is not a terminal; aoe requires an interactive TTY");
    }

    // Mosh mangles mouse-tracking escapes, so capture stays off under it.
    // `App` re-resolves the config so a mid-session toggle still applies.
    let mosh_active = std::env::var_os("MOSH_CONNECTION").is_some();
    let startup_session_config = crate::session::resolve_config(profile)
        .map(|c| c.session)
        .unwrap_or_default();
    crate::session::poller::configure_session_id_poller_max_threads(
        startup_session_config.session_id_poller_max_threads,
    );
    let enable_mouse = mouse_capture_requested(&startup_session_config) && !mosh_active;
    let _terminal_guard = TerminalGuard::enter(enable_mouse, mosh_active)?;

    // Config warnings have no tracing subscriber in TUI mode, so they surface
    // as a dialog. Known before `App::new` so first-run dialogs are suppressed
    // and a malformed config.toml isn't overwritten with defaults.
    let combined_warning = match (
        startup_warning,
        crate::session::collect_startup_config_warnings(profile),
    ) {
        (Some(a), Some(b)) => Some(format!("{a}\n\n{b}")),
        (a, b) => a.or(b),
    };

    // Without file watching, the 5s heartbeat is the only reload signal.
    let file_watch = crate::file_watch::FileWatchService::new().unwrap_or_else(|e| {
        tracing::warn!(
            target: "tui.file_watch",
            error = %e,
            "FileWatchService::new failed; live propagation disabled, falling back to 5s heartbeat"
        );
        crate::file_watch::FileWatchService::noop()
    });

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
    // Shares the hyperlink map the renderer fills.
    let backend = crate::tui::hyperlink::HyperlinkBackend::new(io::stdout(), app.hyperlink_cells());
    let mut terminal = Terminal::new(backend)?;
    let result = app.run(&mut terminal).await;

    crate::session::clear_tui_heartbeat();

    // `_terminal_guard` restores the terminal on drop.
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

    /// The diff baseline must reset too, or an identical redraw paints nothing.
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
    use crate::session::test_support::EnvGuard;
    use serial_test::serial;

    #[test]
    #[serial]
    fn config_and_env_must_both_allow_capture() {
        // (config, AOE_MOUSE_CAPTURE, expected)
        let cases = [
            (true, None, true),
            (false, None, false),
            (true, Some("0"), false),
            (true, Some("false"), false),
            (false, Some("1"), false),
        ];
        for (mouse_capture, env, expected) in cases {
            let _g = match env {
                Some(value) => EnvGuard::set(&[("AOE_MOUSE_CAPTURE", value)]),
                None => EnvGuard::unset(&["AOE_MOUSE_CAPTURE"]),
            };
            let session = SessionConfig {
                mouse_capture,
                ..SessionConfig::default()
            };
            assert_eq!(mouse_capture_requested(&session), expected, "{env:?}");
        }
    }
}
