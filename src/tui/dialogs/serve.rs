//! Serve dialog: drives the `aoe serve --daemon` lifecycle (either Local
//! network mode on 0.0.0.0, or Cloudflare Tunnel mode) and shows a QR +
//! URL + (passphrase for Tunnel) + log tail so a phone can connect. The
//! TUI is a controller here, not a host: it spawns the daemon, reads
//! `$APP_DIR/serve.{pid,url,log,mode}` files, and runs `aoe serve --stop`
//! to tear down. The daemon survives across TUI quits, just like tmux
//! sessions or the CLI-invoked daemon path.
//!
//! Only compiled with the `serve` feature, since the tunnel integration
//! (and the qrcode crate it needs) lives there.
#![cfg(feature = "serve")]

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent};
use qrcode::render::unicode::Dense1x2;
use qrcode::QrCode;
use rand::prelude::IndexedRandom;
use rand::RngExt;
use ratatui::prelude::*;
use ratatui::widgets::*;

use crate::tui::styles::Theme;

/// Actions returned by [`ServeView::handle_key`], following the
/// full-page takeover pattern used by `SettingsAction` and `DiffAction`.
pub enum ServeAction {
    /// Keep the serve view open; no navigation change.
    Continue,
    /// Close the serve view and return to the home screen.
    Close,
}

/// Which transport the daemon is serving over. Persisted to
/// `$APP_DIR/serve.mode` so a reattaching TUI can render the right label
/// and the right set of controls (Tab to cycle is Local-only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServeMode {
    Local,
    Tunnel,
}

/// Which HTTPS tunnel backend the user picked on the Confirm screen.
/// Tunnel mode always has one of these; Local mode doesn't use them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TunnelTransport {
    Tailscale,
    Cloudflare,
}

/// Per-transport readiness, evaluated when the Confirm screen opens.
/// Drives the card styling and whether the user can select that card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportStatus {
    /// Ready to spawn: CLI installed, logged in, and (for Tailscale)
    /// the `funnel` nodeAttr is granted.
    Ready,
    /// CLI is missing on PATH.
    NotInstalled,
    /// Tailscale-only: CLI + login OK but the ACL doesn't grant Funnel.
    /// User needs to visit login.tailscale.com/admin/acls/file.
    FunnelNotEnabled,
}

impl TransportStatus {
    fn is_ready(self) -> bool {
        matches!(self, TransportStatus::Ready)
    }
}

impl ServeMode {
    fn file_token(self) -> &'static str {
        match self {
            ServeMode::Local => "local",
            ServeMode::Tunnel => "tunnel",
        }
    }

    fn from_file_token(s: &str) -> Option<Self> {
        match s.trim() {
            "local" => Some(ServeMode::Local),
            "tunnel" => Some(ServeMode::Tunnel),
            _ => None,
        }
    }
}

pub use crate::cli::serve::{read_serve_urls, ServeUrl};

/// Passphrase cache for daemons this TUI process spawned, so reopening
/// the Remote Access dialog after closing it can re-display the same
/// passphrase instead of the "set at startup" placeholder. Cleared when
/// the daemon is stopped from this process. A daemon spawned by a
/// separate `aoe serve` invocation (outside this TUI) leaves this None,
/// so we correctly fall back to the placeholder for those.
static LAST_SPAWNED_PASSPHRASE: Mutex<Option<String>> = Mutex::new(None);

fn remember_passphrase(pp: &str) {
    if let Ok(mut guard) = LAST_SPAWNED_PASSPHRASE.lock() {
        *guard = Some(pp.to_string());
    }
}

fn recall_passphrase_in_memory() -> Option<String> {
    LAST_SPAWNED_PASSPHRASE.lock().ok()?.clone()
}

fn recall_passphrase() -> Option<String> {
    if let Some(pp) = recall_passphrase_in_memory() {
        tracing::debug!(target: "tui.dialog", "passphrase recalled from in-memory cache");
        return Some(pp);
    }
    // Durable saved passphrase (survives stop/start cycles).
    if let Some(pp) = load_saved_passphrase() {
        tracing::debug!(target: "tui.dialog", "passphrase recalled from serve.saved_passphrase");
        return Some(pp);
    }
    // Ephemeral file written by the server on startup. Lets the TUI
    // display the passphrase when the daemon was launched from the CLI.
    let dir = crate::session::get_app_dir().ok()?;
    let raw = std::fs::read_to_string(dir.join("serve.passphrase")).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        tracing::debug!(target: "tui.dialog", "passphrase recalled from serve.passphrase on disk");
        Some(trimmed.to_string())
    }
}

fn forget_passphrase() {
    forget_passphrase_in_memory();
    // Only remove the ephemeral file. The durable saved_passphrase
    // intentionally survives stop/start so the same passphrase can
    // be reused on the next launch.
    if let Ok(dir) = crate::session::get_app_dir() {
        let _ = std::fs::remove_file(dir.join("serve.passphrase"));
    }
}

/// Load the durable saved passphrase that persists across daemon
/// stop/start cycles. Returns None if no saved passphrase exists.
fn load_saved_passphrase() -> Option<String> {
    let dir = crate::session::get_app_dir().ok()?;
    let raw = std::fs::read_to_string(dir.join("serve.saved_passphrase")).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Persist a passphrase to the durable file that survives daemon
/// stop/start cycles. Written with owner-only permissions.
fn save_passphrase_to_disk(pp: &str) {
    if let Ok(dir) = crate::session::get_app_dir() {
        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(dir.join("serve.saved_passphrase"))
            {
                let _ = file.write_all(pp.as_bytes());
            }
        }
        #[cfg(not(unix))]
        {
            let _ = std::fs::write(dir.join("serve.saved_passphrase"), pp);
        }
    }
}

/// Load the saved passphrase if one exists, otherwise generate a
/// fresh random one and save it for future launches.
fn load_or_generate_passphrase() -> String {
    if let Some(pp) = load_saved_passphrase() {
        return pp;
    }
    let pp = generate_passphrase();
    save_passphrase_to_disk(&pp);
    pp
}

fn forget_passphrase_in_memory() {
    if let Ok(mut guard) = LAST_SPAWNED_PASSPHRASE.lock() {
        *guard = None;
    }
}

/// How long we wait for `serve.url` to appear after spawning the daemon.
const TUNNEL_STARTUP_TIMEOUT_SECS: u64 = 60;
/// How much of the configured log file to keep in memory for the tail pane.
const LOG_TAIL_LINES: usize = 200;

pub enum ServeViewState {
    /// No daemon running; first screen the user sees. They pick Local
    /// (bind 0.0.0.0, token auth only) or Tunnel (cloudflared + passphrase).
    /// `tunnel_available` gates the Tunnel card; `local_available`
    /// is false when the host has no non-loopback interface (dockerized
    /// dev env with only lo).
    ModePicker {
        selected: ServeMode,
        /// Either tailscale OR cloudflared is available. Gate for the
        /// Tunnel card; actual transport choice happens on Confirm.
        tunnel_available: bool,
        local_available: bool,
        /// Transient flash message shown for ~1s after a rejected keypress
        /// (e.g., picking Tunnel when no tunnel tool is installed).
        flash: Option<(String, Instant)>,
    },
    /// Tunnel-only: risk explanation AND transport picker on one screen.
    /// User sees security implications + chooses Tailscale vs Cloudflare
    /// with up-front readiness info, no mid-spawn surprises. Local mode
    /// never enters Confirm — it goes ModePicker → Starting directly.
    Confirm {
        /// Which transport card is currently highlighted.
        selected: TunnelTransport,
        /// Readiness evaluated when the screen opened; refreshable via [R].
        tailscale: TransportStatus,
        cloudflare: TransportStatus,
        /// Transient message (e.g. "opened admin console").
        flash: Option<(String, Instant)>,
    },
    /// We issued `aoe serve --daemon`; now polling `serve.url`.
    /// `passphrase` is Some only for Tunnel spawns from this TUI.
    /// `log_tail` is a rolling window of the configured log file (since the
    /// captured offset taken before spawn) so the user sees real progress
    /// during the 30-60s cert-provisioning wait on a fresh Tailscale node
    /// instead of a frozen screen.
    Starting {
        mode: ServeMode,
        /// Remembered for restart. None for Local mode or external daemons.
        transport: Option<TunnelTransport>,
        passphrase: Option<String>,
        started_at: Instant,
        log_tail: Vec<String>,
        log_offset: u64,
    },
    /// Daemon is live. No child field — the TUI does not own it.
    Active {
        mode: ServeMode,
        /// Which tunnel transport was used (remembered from the Confirm
        /// screen so we can pass it to restart). None for Local mode or
        /// daemons started externally.
        transport: Option<TunnelTransport>,
        urls: Vec<ServeUrl>,
        /// Which `urls` entry is the primary QR target. Starts at 0.
        /// Tab advances; cycles; no-op when urls.len() <= 1.
        url_index: usize,
        /// Only known when this TUI started the daemon. For daemons
        /// started via the CLI we show a "set at startup" placeholder.
        /// Always None for Local mode.
        passphrase: Option<String>,
        opened_at: Instant,
        log_tail: Vec<String>,
        /// Last-seen log-file length so we only read appended bytes.
        log_offset: u64,
    },
    Error(String),
}

/// A destructive action awaiting confirmation (press the key again).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingConfirm {
    /// [G] pressed once: will generate a new random passphrase + restart.
    NewPassphrase,
    /// [R] pressed once: will restart the server (clears all sessions).
    Restart,
}

pub struct ServeView {
    state: ServeViewState,
    /// Passphrase we will use if the user picks Tunnel and confirms.
    /// Loaded from `serve.saved_passphrase` if available, otherwise
    /// freshly generated and saved.
    pending_passphrase: String,
    /// Destructive action awaiting a second keypress to confirm.
    /// Cleared on any other key or after a timeout rendered in the footer.
    pending_confirm: Option<(PendingConfirm, Instant)>,
    /// Whether the help overlay is visible.
    show_help: bool,
}

impl Default for ServeView {
    fn default() -> Self {
        Self::new()
    }
}

impl ServeView {
    /// Construct the dialog. If a daemon is already running (detected via
    /// `$APP_DIR/serve.pid`), jump straight to Active so the user can see
    /// the URL and stop it; otherwise show ModePicker.
    pub fn new() -> Self {
        // Use the saved passphrase if one exists, otherwise generate
        // a fresh one and save it. This ensures the passphrase stays
        // constant across stop/start cycles.
        let pending = load_or_generate_passphrase();

        if crate::cli::serve::daemon_pid().is_some() {
            // There's already a daemon running. Read its mode from
            // serve.mode (written by the server). If missing (older daemon
            // from pre-mode-split version), assume Tunnel — that was the
            // only mode the TUI could spawn before.
            let mode = read_serve_mode().unwrap_or(ServeMode::Tunnel);
            // Recall the passphrase only for Tunnel (Local has no passphrase).
            let remembered = if matches!(mode, ServeMode::Tunnel) {
                recall_passphrase()
            } else {
                None
            };
            let urls = read_serve_urls();
            if urls.is_empty() {
                Self {
                    state: ServeViewState::Starting {
                        mode,
                        transport: None, // unknown for reattached daemons
                        passphrase: remembered,
                        started_at: Instant::now(),
                        log_tail: initial_log_tail(),
                        log_offset: log_file_size(),
                    },
                    pending_passphrase: pending,
                    pending_confirm: None,
                    show_help: false,
                }
            } else {
                Self {
                    state: ServeViewState::Active {
                        mode,
                        transport: None, // unknown for reattached daemons
                        urls,
                        url_index: 0,
                        passphrase: remembered,
                        opened_at: Instant::now(),
                        log_tail: initial_log_tail(),
                        log_offset: log_file_size(),
                    },
                    pending_passphrase: pending,
                    pending_confirm: None,
                    show_help: false,
                }
            }
        } else {
            // Tunnel mode is usable if EITHER a logged-in Tailscale or
            // cloudflared is installed. Transport-specific readiness is
            // re-evaluated when the user reaches the Confirm screen.
            let tailscale_ok = crate::server::tunnel::tailscale_available_sync();
            let cloudflared_ok = crate::server::tunnel::check_cloudflared().is_ok();
            let tunnel_available = tailscale_ok || cloudflared_ok;
            let local_available = !crate::server::discover_tagged_ips().is_empty();
            // Default highlight: the last mode the user successfully
            // launched (read from serve.last_mode). Fall back to Local as
            // safer first-time default. If Local isn't actually available,
            // prefer Tunnel (and vice versa for cloudflared-missing).
            let remembered_default = read_last_mode().unwrap_or(ServeMode::Local);
            let selected = match remembered_default {
                ServeMode::Local if local_available => ServeMode::Local,
                ServeMode::Local if tunnel_available => ServeMode::Tunnel,
                ServeMode::Tunnel if tunnel_available => ServeMode::Tunnel,
                ServeMode::Tunnel if local_available => ServeMode::Local,
                _ => ServeMode::Local, // no-op default when neither works; picker handles it
            };
            Self {
                state: ServeViewState::ModePicker {
                    selected,
                    tunnel_available,
                    local_available,
                    flash: None,
                },
                pending_passphrase: pending,
                pending_confirm: None,
                show_help: false,
            }
        }
    }

    /// Probe installed tunnel backends and return their readiness for
    /// the Confirm screen. Called when entering Confirm and when the
    /// user presses [R] to refresh after fixing an ACL.
    fn assess_transports() -> (TransportStatus, TransportStatus) {
        let tailscale = if !crate::server::tunnel::tailscale_available_sync() {
            TransportStatus::NotInstalled
        } else if !crate::server::tunnel::tailscale_funnel_cap_ready_sync() {
            TransportStatus::FunnelNotEnabled
        } else {
            TransportStatus::Ready
        };
        let cloudflare = if crate::server::tunnel::check_cloudflared().is_ok() {
            TransportStatus::Ready
        } else {
            TransportStatus::NotInstalled
        };
        (tailscale, cloudflare)
    }

    /// Pick the default-highlighted transport on Confirm: a Ready
    /// Tailscale beats Cloudflare (stable URL wins by default); else
    /// fall back to whichever is Ready; else default to Tailscale so
    /// the user sees the fix instructions (they came here on purpose).
    fn default_transport(
        tailscale: TransportStatus,
        cloudflare: TransportStatus,
    ) -> TunnelTransport {
        match (tailscale, cloudflare) {
            (TransportStatus::Ready, _) => TunnelTransport::Tailscale,
            (_, TransportStatus::Ready) => TunnelTransport::Cloudflare,
            _ => TunnelTransport::Tailscale,
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> ServeAction {
        match &mut self.state {
            ServeViewState::ModePicker {
                selected,
                tunnel_available,
                local_available,
                flash,
            } => {
                // Helper: attempt to commit the current `selected` mode,
                // transitioning to Confirm (Tunnel) or Starting (Local).
                // Rejects with a flash message if the mode isn't available.
                let commit = |dialog: &mut ServeView| -> ServeAction {
                    let ServeViewState::ModePicker {
                        selected,
                        tunnel_available,
                        local_available,
                        ..
                    } = &dialog.state
                    else {
                        return ServeAction::Continue;
                    };
                    let mode = *selected;
                    let cf = *tunnel_available;
                    let la = *local_available;
                    match mode {
                        ServeMode::Tunnel if !cf => {
                            if let ServeViewState::ModePicker { flash, .. } = &mut dialog.state {
                                *flash = Some((
                                    "Install tailscale or cloudflared to enable Tunnel mode."
                                        .to_string(),
                                    Instant::now(),
                                ));
                            }
                            ServeAction::Continue
                        }
                        ServeMode::Local if !la => {
                            if let ServeViewState::ModePicker { flash, .. } = &mut dialog.state {
                                *flash = Some((
                                    "No non-loopback network interface available.".to_string(),
                                    Instant::now(),
                                ));
                            }
                            ServeAction::Continue
                        }
                        ServeMode::Tunnel => {
                            let (tailscale, cloudflare) = ServeView::assess_transports();
                            let selected = ServeView::default_transport(tailscale, cloudflare);
                            dialog.state = ServeViewState::Confirm {
                                selected,
                                tailscale,
                                cloudflare,
                                flash: None,
                            };
                            ServeAction::Continue
                        }
                        ServeMode::Local => {
                            // Capture the offset before spawn so the tail
                            // pane starts at the byte boundary just past any
                            // pre-existing TUI/runner content; the new
                            // daemon's startup marker and first events are
                            // appended past this point and stream in via
                            // append_new_log_lines.
                            let offset = log_file_size();
                            match spawn_daemon(ServeMode::Local, None, None) {
                                Ok(()) => {
                                    remember_last_mode(ServeMode::Local);
                                    dialog.state = ServeViewState::Starting {
                                        mode: ServeMode::Local,
                                        transport: None,
                                        passphrase: None,
                                        started_at: Instant::now(),
                                        log_tail: initial_log_tail(),
                                        log_offset: offset,
                                    };
                                }
                                Err(e) => dialog.state = ServeViewState::Error(e),
                            }
                            ServeAction::Continue
                        }
                    }
                };

                // Clear stale flash on any key press (helps the user feel
                // they're making progress even if the next key is invalid).
                if flash
                    .as_ref()
                    .map(|(_, t)| t.elapsed() > Duration::from_millis(1500))
                    .unwrap_or(false)
                {
                    *flash = None;
                }

                match key.code {
                    KeyCode::Left | KeyCode::Char('h') => {
                        *selected = ServeMode::Local;
                        ServeAction::Continue
                    }
                    KeyCode::Right | KeyCode::Char('l') => {
                        // Only move to Tunnel if it's usable; otherwise
                        // keep Local selected (don't let the user park
                        // the cursor on a dimmed card).
                        if *tunnel_available {
                            *selected = ServeMode::Tunnel;
                        }
                        ServeAction::Continue
                    }
                    KeyCode::Tab => {
                        *selected = match *selected {
                            ServeMode::Local if *tunnel_available => ServeMode::Tunnel,
                            ServeMode::Tunnel if *local_available => ServeMode::Local,
                            other => other,
                        };
                        ServeAction::Continue
                    }
                    KeyCode::Char('t') | KeyCode::Char('T') => {
                        *selected = ServeMode::Tunnel;
                        commit(self)
                    }
                    KeyCode::Char('L') => {
                        // Capital L as the explicit-Local shortcut. Keep
                        // lowercase `l` as "→ move right" per the arrow-key
                        // parallel above, which is the existing convention
                        // in the rest of the TUI.
                        *selected = ServeMode::Local;
                        commit(self)
                    }
                    KeyCode::Enter => commit(self),
                    KeyCode::Esc | KeyCode::Char('q') => ServeAction::Close,
                    _ => ServeAction::Continue,
                }
            }
            ServeViewState::Confirm {
                selected,
                tailscale,
                cloudflare,
                flash,
            } => {
                // Expire stale flash on next key.
                if flash
                    .as_ref()
                    .map(|(_, t)| t.elapsed() > Duration::from_millis(1500))
                    .unwrap_or(false)
                {
                    *flash = None;
                }

                // Helper: commit the currently-selected transport. Rejects
                // with a flash if the selected card isn't Ready (keeps the
                // user on the Confirm screen where they can pivot to the
                // other transport or hit [E]).
                let commit = |dialog: &mut ServeView| -> ServeAction {
                    let ServeViewState::Confirm {
                        selected,
                        tailscale,
                        cloudflare,
                        ..
                    } = &dialog.state
                    else {
                        return ServeAction::Continue;
                    };
                    let pick = *selected;
                    let status = match pick {
                        TunnelTransport::Tailscale => *tailscale,
                        TunnelTransport::Cloudflare => *cloudflare,
                    };
                    if !status.is_ready() {
                        let msg = match (pick, status) {
                            (TunnelTransport::Tailscale, TransportStatus::FunnelNotEnabled) => {
                                "Tailscale Funnel isn't enabled for this node; pick Cloudflare or update your ACL."
                            }
                            (TunnelTransport::Tailscale, _) => {
                                "Tailscale isn't installed; pick Cloudflare."
                            }
                            (TunnelTransport::Cloudflare, _) => {
                                "cloudflared isn't installed; pick Tailscale."
                            }
                        };
                        if let ServeViewState::Confirm { flash, .. } = &mut dialog.state {
                            *flash = Some((msg.to_string(), Instant::now()));
                        }
                        return ServeAction::Continue;
                    }
                    // Capture offset before spawn so the tail pane streams in
                    // only the new daemon's startup events.
                    let offset = log_file_size();
                    match spawn_daemon(
                        ServeMode::Tunnel,
                        Some(&dialog.pending_passphrase),
                        Some(pick),
                    ) {
                        Ok(()) => {
                            remember_last_mode(ServeMode::Tunnel);
                            dialog.state = ServeViewState::Starting {
                                mode: ServeMode::Tunnel,
                                transport: Some(pick),
                                passphrase: Some(dialog.pending_passphrase.clone()),
                                started_at: Instant::now(),
                                log_tail: initial_log_tail(),
                                log_offset: offset,
                            };
                        }
                        Err(e) => dialog.state = ServeViewState::Error(e),
                    }
                    ServeAction::Continue
                };

                match key.code {
                    KeyCode::Left | KeyCode::Char('h') => {
                        *selected = TunnelTransport::Tailscale;
                        ServeAction::Continue
                    }
                    KeyCode::Right | KeyCode::Char('l') => {
                        *selected = TunnelTransport::Cloudflare;
                        ServeAction::Continue
                    }
                    KeyCode::Tab => {
                        *selected = match *selected {
                            TunnelTransport::Tailscale => TunnelTransport::Cloudflare,
                            TunnelTransport::Cloudflare => TunnelTransport::Tailscale,
                        };
                        ServeAction::Continue
                    }
                    KeyCode::Char('t') | KeyCode::Char('T') => {
                        *selected = TunnelTransport::Tailscale;
                        commit(self)
                    }
                    KeyCode::Char('c') | KeyCode::Char('C') => {
                        *selected = TunnelTransport::Cloudflare;
                        commit(self)
                    }
                    KeyCode::Enter => commit(self),
                    KeyCode::Char('r') | KeyCode::Char('R') => {
                        let (new_ts, new_cf) = ServeView::assess_transports();
                        *tailscale = new_ts;
                        *cloudflare = new_cf;
                        *flash = Some(("Refreshed.".to_string(), Instant::now()));
                        ServeAction::Continue
                    }
                    KeyCode::Esc | KeyCode::Char('q') => ServeAction::Close,
                    _ => ServeAction::Continue,
                }
            }
            ServeViewState::Starting { .. } => match key.code {
                // Esc just closes the dialog; the daemon keeps coming up.
                KeyCode::Esc | KeyCode::Char('q') => ServeAction::Close,
                KeyCode::Char('s') | KeyCode::Char('S') => {
                    // Aborting startup: stop the (half-started) daemon.
                    let _ = stop_daemon();
                    ServeAction::Close
                }
                _ => ServeAction::Continue,
            },
            ServeViewState::Active {
                mode,
                transport,
                urls,
                url_index,
                passphrase,
                ..
            } => {
                // Help overlay intercepts all keys when visible.
                if self.show_help {
                    self.show_help = false;
                    return ServeAction::Continue;
                }

                // Check pending confirmation inline (can't call &mut self
                // method while self.state is borrowed by the match arm).
                let confirmed = if let Some((action, when)) = self.pending_confirm {
                    if when.elapsed() > Duration::from_secs(3) {
                        self.pending_confirm = None;
                        None
                    } else {
                        let matches = match action {
                            PendingConfirm::NewPassphrase => {
                                matches!(key.code, KeyCode::Char('g') | KeyCode::Char('G'))
                            }
                            PendingConfirm::Restart => {
                                matches!(key.code, KeyCode::Char('r') | KeyCode::Char('R'))
                            }
                        };
                        if matches {
                            Some(action)
                        } else {
                            self.pending_confirm = None;
                            None
                        }
                    }
                } else {
                    None
                };

                match key.code {
                    // Stop: transition to ModePicker so the user can restart
                    // with different settings. Esc/q is the way to close.
                    KeyCode::Char('s') | KeyCode::Char('S') => match stop_daemon() {
                        Ok(()) => {
                            self.reset_to_mode_picker();
                            ServeAction::Continue
                        }
                        Err(e) => {
                            self.state = ServeViewState::Error(format!(
                                "Stop failed: {}. Daemon may still be running; retry or use `aoe serve --stop` from a shell.",
                                e
                            ));
                            ServeAction::Continue
                        }
                    },
                    // Generate new random passphrase + restart (Tunnel only).
                    // First press shows confirmation, second press executes.
                    KeyCode::Char('g') | KeyCode::Char('G')
                        if matches!(mode, ServeMode::Tunnel) =>
                    {
                        if confirmed == Some(PendingConfirm::NewPassphrase) {
                            let new_pp = generate_passphrase();
                            save_passphrase_to_disk(&new_pp);
                            self.pending_passphrase = new_pp.clone();
                            let m = *mode;
                            let t = *transport;
                            self.pending_confirm = None;
                            self.do_restart(m, t, Some(new_pp));
                        } else {
                            self.pending_confirm =
                                Some((PendingConfirm::NewPassphrase, Instant::now()));
                        }
                        ServeAction::Continue
                    }
                    // Restart server. For Tunnel mode this clears all login
                    // sessions; for Local mode it rebinds the port.
                    // First press shows confirmation, second press executes.
                    KeyCode::Char('r') | KeyCode::Char('R') => {
                        if confirmed == Some(PendingConfirm::Restart) {
                            let pp = if matches!(mode, ServeMode::Tunnel) {
                                let s = passphrase.as_deref().unwrap_or(&self.pending_passphrase);
                                Some(s.to_string())
                            } else {
                                None
                            };
                            let m = *mode;
                            let t = *transport;
                            self.pending_confirm = None;
                            self.do_restart(m, t, pp);
                        } else {
                            self.pending_confirm = Some((PendingConfirm::Restart, Instant::now()));
                        }
                        ServeAction::Continue
                    }
                    KeyCode::Tab if urls.len() > 1 => {
                        *url_index = (*url_index + 1) % urls.len();
                        ServeAction::Continue
                    }
                    KeyCode::Char('?') => {
                        self.show_help = true;
                        self.pending_confirm = None;
                        ServeAction::Continue
                    }
                    KeyCode::Esc | KeyCode::Char('q') => ServeAction::Close,
                    _ => ServeAction::Continue,
                }
            }
            ServeViewState::Error(msg) => match key.code {
                KeyCode::Char('s') | KeyCode::Char('S') => {
                    // Best-effort stop for a daemon that may still be
                    // lingering. Ignore the result — if there's no daemon
                    // to stop, that's the desired state anyway.
                    let _ = stop_daemon();
                    ServeAction::Close
                }
                KeyCode::Char('r') | KeyCode::Char('R') if error_mentions_tailscale(msg) => {
                    // Tailscale-related error: offer one-shot recovery
                    // via `tailscale funnel reset`. Common triggers are a
                    // stale non-loopback funnel config blocking port 443,
                    // or a half-configured funnel the user wants to wipe.
                    // Reset is safe even when the funnel isn't configured.
                    let result = run_tailscale_funnel_reset();
                    self.state = match result {
                        Ok(()) => ServeViewState::Error(
                            "Ran `tailscale funnel reset`. The existing funnel \
                             config (if any) has been cleared.\n\n\
                             Close this dialog and press R to retry."
                                .to_string(),
                        ),
                        Err(e) => ServeViewState::Error(format!(
                            "`tailscale funnel reset` failed: {e}\n\n\
                             Try running it manually from a shell, then retry."
                        )),
                    };
                    ServeAction::Continue
                }
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Char('q') => {
                    ServeAction::Close
                }
                _ => ServeAction::Continue,
            },
        }
    }

    /// Restart the daemon and transition to Starting (or directly to
    /// Active if the server comes up fast enough to avoid a flash).
    fn do_restart(
        &mut self,
        mode: ServeMode,
        transport: Option<TunnelTransport>,
        passphrase: Option<String>,
    ) {
        let pp_ref = passphrase.as_deref();
        // Capture offset before restart so the tail pane streams in only
        // the new daemon's events.
        let offset = log_file_size();
        match restart_daemon(mode, pp_ref, transport) {
            Ok(()) => {
                if let Some(ref pp) = passphrase {
                    remember_passphrase(pp);
                }
                // If the server comes back fast (Tailscale reusing an
                // existing tunnel), skip the Starting flash entirely.
                // Give it a brief moment then check for the URL file.
                std::thread::sleep(Duration::from_millis(200));
                let urls = read_serve_urls();
                if !urls.is_empty() {
                    self.state = ServeViewState::Active {
                        mode,
                        transport,
                        urls,
                        url_index: 0,
                        passphrase,
                        opened_at: Instant::now(),
                        log_tail: initial_log_tail(),
                        log_offset: offset,
                    };
                } else {
                    self.state = ServeViewState::Starting {
                        mode,
                        transport,
                        passphrase,
                        started_at: Instant::now(),
                        log_tail: initial_log_tail(),
                        log_offset: offset,
                    };
                }
            }
            Err(e) => {
                self.state = ServeViewState::Error(format!("Restart failed: {}", e));
            }
        }
    }

    /// Reset to the mode picker (used after stopping the daemon so
    /// the user can restart with different settings instead of being
    /// kicked back to the home screen).
    fn reset_to_mode_picker(&mut self) {
        let tailscale_ok = crate::server::tunnel::tailscale_available_sync();
        let cloudflared_ok = crate::server::tunnel::check_cloudflared().is_ok();
        let tunnel_available = tailscale_ok || cloudflared_ok;
        let local_available = !crate::server::discover_tagged_ips().is_empty();
        let remembered_default = read_last_mode().unwrap_or(ServeMode::Local);
        let selected = match remembered_default {
            ServeMode::Local if local_available => ServeMode::Local,
            ServeMode::Local if tunnel_available => ServeMode::Tunnel,
            ServeMode::Tunnel if tunnel_available => ServeMode::Tunnel,
            ServeMode::Tunnel if local_available => ServeMode::Local,
            _ => ServeMode::Local,
        };
        self.state = ServeViewState::ModePicker {
            selected,
            tunnel_available,
            local_available,
            flash: None,
        };
        self.pending_confirm = None;
        self.show_help = false;
    }

    /// Poll files on disk and drive state transitions. Returns true when
    /// the visible state changed and a redraw is needed.
    pub fn tick(&mut self) -> bool {
        match &mut self.state {
            ServeViewState::ModePicker { flash, .. } => {
                // Expire the flash message after 1.5s so it doesn't stick
                // around forever without a follow-up key press.
                if let Some((_, t)) = flash {
                    if t.elapsed() > Duration::from_millis(1500) {
                        *flash = None;
                        return true;
                    }
                }
                false
            }
            ServeViewState::Starting {
                mode,
                transport,
                passphrase,
                started_at,
                log_tail,
                log_offset,
            } => {
                // Tail the configured log file so the user watches real
                // progress (cert generation, tunnel handshake, etc.)
                // during the 30-60s wait instead of a frozen screen.
                let log_changed = append_new_log_lines(log_tail, log_offset);
                let mode = *mode;
                let xport = *transport;
                let urls = read_serve_urls();
                if !urls.is_empty() {
                    // If we entered Starting without a passphrase (e.g.
                    // the TUI reattached to a daemon started elsewhere),
                    // retry the disk fallback now that the server has had
                    // time to finish startup and write serve.passphrase.
                    let pp = match passphrase.clone() {
                        Some(pp) => Some(pp),
                        None if matches!(mode, ServeMode::Tunnel) => recall_passphrase(),
                        None => None,
                    };
                    self.state = ServeViewState::Active {
                        mode,
                        transport: xport,
                        urls,
                        url_index: 0,
                        passphrase: pp,
                        opened_at: Instant::now(),
                        log_tail: initial_log_tail(),
                        log_offset: log_file_size(),
                    };
                    return true;
                }
                // If the daemon process dies before writing serve.url,
                // fail fast with the last few log lines so the user can see
                // why. Common Local mode causes: port in use, EADDRNOTAVAIL
                // (Tailscale iface went away), permission denied.
                if crate::cli::serve::daemon_pid().is_none() {
                    let tail = initial_log_tail();
                    let raw_joined = tail.join("\n");
                    let hint = diagnose_daemon_exit(&raw_joined, mode);
                    let compact: Vec<String> = tail.iter().map(|l| compact_log_line(l)).collect();
                    let detail = if compact.is_empty() {
                        String::new()
                    } else {
                        format!("\n\nLast log lines:\n{}", compact.join("\n"))
                    };
                    let prefix = match mode {
                        ServeMode::Tunnel => {
                            "`aoe serve --remote --daemon` exited before the tunnel came up."
                        }
                        ServeMode::Local => {
                            "`aoe serve --daemon` exited before the server started."
                        }
                    };
                    self.state = ServeViewState::Error(format!("{}{}{}", prefix, hint, detail));
                    return true;
                }
                // Local mode comes up ~instantly; no need for the 60s
                // cloudflared-timeout path. Tunnel mode keeps it.
                if matches!(mode, ServeMode::Tunnel)
                    && started_at.elapsed() > Duration::from_secs(TUNNEL_STARTUP_TIMEOUT_SECS)
                {
                    // Timeout: the daemon is alive but never produced a
                    // tunnel URL (cloudflared rate-limited, captive portal,
                    // etc.). Stop it now so we don't leave a zombie that
                    // can never serve phones but keeps tripping the status
                    // bar indicator. Fall through to a log-tail error view.
                    let stop_note = match stop_daemon() {
                        Ok(()) => "Stuck daemon stopped.".to_string(),
                        Err(e) => format!(
                            "Daemon may still be running \
                             (tried to stop: {}). Stop manually with `aoe serve --stop`.",
                            e
                        ),
                    };
                    let tail = initial_log_tail();
                    let compact: Vec<String> = tail.iter().map(|l| compact_log_line(l)).collect();
                    let tail_detail = if compact.is_empty() {
                        String::new()
                    } else {
                        format!("\n\nLast log lines:\n{}", compact.join("\n"))
                    };
                    self.state = ServeViewState::Error(format!(
                        "HTTPS tunnel did not announce a URL within {}s. \
                         {}\n\n\
                         Most likely cause: Tailscale Funnel needs HTTPS certs \
                         or ACL approval, OR cloudflared is rate-limited / \
                         offline. Re-run with AGENT_OF_EMPIRES_DEBUG=1 and \
                         check debug.log for details.{}",
                        TUNNEL_STARTUP_TIMEOUT_SECS, stop_note, tail_detail
                    ));
                    return true;
                }
                log_changed
            }
            ServeViewState::Active {
                log_tail,
                log_offset,
                ..
            } => {
                let log_changed = append_new_log_lines(log_tail, log_offset);
                // Expire stale pending confirmation so the footer hint
                // disappears after 3s even without a keypress.
                let confirm_expired = self
                    .pending_confirm
                    .as_ref()
                    .map(|(_, t)| t.elapsed() > Duration::from_secs(3))
                    .unwrap_or(false);
                if confirm_expired {
                    self.pending_confirm = None;
                    return true;
                }
                log_changed
            }
            _ => false,
        }
    }

    pub fn render(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        match &self.state {
            ServeViewState::ModePicker {
                selected,
                tunnel_available,
                local_available,
                flash,
            } => render_mode_picker(
                frame,
                area,
                theme,
                *selected,
                *tunnel_available,
                *local_available,
                flash.as_ref().map(|(m, _)| m.as_str()),
            ),
            ServeViewState::Confirm {
                selected,
                tailscale,
                cloudflare,
                flash,
            } => render_confirm(
                frame,
                area,
                theme,
                *selected,
                *tailscale,
                *cloudflare,
                flash.as_ref().map(|(m, _)| m.as_str()),
            ),
            ServeViewState::Starting {
                mode, started_at, ..
            } => render_starting(frame, area, theme, *mode, started_at.elapsed()),
            ServeViewState::Active {
                mode,
                urls,
                url_index,
                passphrase,
                opened_at,
                ..
            } => {
                render_active(
                    frame,
                    area,
                    theme,
                    *mode,
                    urls,
                    *url_index,
                    passphrase.as_deref(),
                    opened_at.elapsed(),
                    self.pending_confirm.as_ref().map(|(a, _)| *a),
                );
                if self.show_help {
                    render_help_overlay(frame, area, theme, *mode);
                }
            }
            ServeViewState::Error(msg) => render_error(frame, area, theme, msg),
        }
    }
}

/// Spawn a localhost daemon (the same Local mode as the serve dialog's
/// quick-start) and poll until discovery resolves it AND it answers a
/// health check, so callers can drive the daemon API immediately after
/// this returns. Used by the structured-view entry points to turn the
/// old "no structured view daemon is running" dead end into a one-key
/// recovery; remote modes stay behind the full serve dialog.
pub(crate) async fn start_local_daemon_and_wait(
) -> Result<crate::acp::client::DaemonEndpoint, String> {
    use crate::acp::client::{discovery::discover, HttpClient};

    spawn_daemon(ServeMode::Local, None, None)?;
    // The daemonized child double-forks, binds, then writes serve.url;
    // ~1s on a warm start. 20s covers a cold start on a slow disk
    // without wedging the UI forever if the daemon dies mid-boot.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let ready = match discover() {
            Ok(endpoint) => match HttpClient::new(endpoint.clone()) {
                Ok(client) => client.health_check().await.is_ok().then_some(endpoint),
                Err(_) => None,
            },
            Err(_) => None,
        };
        if let Some(endpoint) = ready {
            return Ok(endpoint);
        }
        if std::time::Instant::now() >= deadline {
            let tail = initial_log_tail();
            let hint = if tail.is_empty() {
                String::new()
            } else {
                format!(" Last log lines:\n{}", tail.join("\n"))
            };
            return Err(format!(
                "the daemon was started but never became reachable.{hint}"
            ));
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
}

/// Spawn the aoe serve daemon in the requested mode. Tunnel requires a
/// passphrase (it's public-internet exposure) and a transport choice;
/// Local ignores both.
fn spawn_daemon(
    mode: ServeMode,
    passphrase: Option<&str>,
    transport: Option<TunnelTransport>,
) -> Result<(), String> {
    use std::process::Command;

    // Guard: refuse to spawn if a daemon is already running. The dialog
    // constructor checks daemon_pid() and skips to Active, but there is
    // a window between that check and reaching here (user navigating
    // ModePicker). A spawn here would overwrite the PID file and orphan
    // the existing daemon.
    if crate::cli::serve::daemon_pid().is_some() {
        return Err(
            "A daemon is already running. Close this dialog and reopen to see it.".to_string(),
        );
    }

    let exe =
        std::env::current_exe().map_err(|e| format!("Could not resolve aoe binary path: {}", e))?;

    // Delete stale serve.url / serve.mode / serve.passphrase from a
    // previous hard-killed daemon before launching. Without this,
    // Starting-state polling could latch onto the old URL before the
    // new daemon writes the new one, and the TUI could briefly display
    // the previous tunnel's passphrase before the new one is written.
    if let Ok(dir) = crate::session::get_app_dir() {
        let _ = std::fs::remove_file(dir.join("serve.url"));
        let _ = std::fs::remove_file(dir.join("serve.mode"));
        let _ = std::fs::remove_file(dir.join("serve.passphrase"));
    }

    // Reuse the port from the last TUI-launched daemon so the user can
    // bookmark the URL and not have to re-paste it after every restart.
    // Only generate a fresh random port on the very first launch (or if
    // the persisted file is missing). This avoids colliding with a user's
    // own `aoe serve` on the default 8080.
    let port: u16 = load_or_generate_port();

    let mut cmd = Command::new(&exe);
    cmd.args(["serve", "--daemon", "--port", &port.to_string()]);
    match mode {
        ServeMode::Tunnel => {
            cmd.args(["--remote", "--host", "127.0.0.1"]);
            // User explicitly picked a transport on the Confirm screen:
            // Cloudflare → force --no-tailscale so the server skips the
            // auto-detect (they may have tailscale installed but chose
            // not to use it). Tailscale → no flag needed; auto-detect
            // will find it.
            if let Some(TunnelTransport::Cloudflare) = transport {
                cmd.arg("--no-tailscale");
            }
            if let Some(pp) = passphrase {
                cmd.env("AOE_SERVE_PASSPHRASE", pp);
            }
        }
        ServeMode::Local => {
            // 0.0.0.0 makes the server reachable on every local
            // interface (Tailscale, LAN, loopback). The server-side
            // serve.url writer picks Tailscale > LAN > localhost as the
            // primary URL in the QR.
            cmd.args(["--host", "0.0.0.0"]);
        }
    }
    cmd.stdin(std::process::Stdio::null())
        // The daemon path forks; the child's tracing + stdio land in the
        // configured log file via stdio_redirect_path. We only need this
        // wrapper's exit status (synchronous, just double-forks).
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    let status = cmd
        .status()
        .map_err(|e| format!("Failed to launch `aoe serve --daemon`: {}", e))?;

    if !status.success() {
        // If the daemon failed because the port was in use, clear the
        // persisted port so the next attempt picks a fresh one instead
        // of getting stuck on the same occupied port forever.
        let tail = initial_log_tail().join("\n");
        if tail.contains("EADDRINUSE") || tail.contains("Address already in use") {
            if let Ok(dir) = crate::session::get_app_dir() {
                let _ = std::fs::remove_file(dir.join("serve.last_port"));
            }
        }

        let hint = match mode {
            ServeMode::Tunnel => format!(
                "Most likely no tunnel tool is installed (install tailscale \
                 or cloudflared) or port {} is in use.",
                port
            ),
            ServeMode::Local => format!("Most likely port {} is in use.", port),
        };
        return Err(format!(
            "`aoe serve --daemon` exited with {:?}. {}",
            status.code(),
            hint
        ));
    }
    if let Some(pp) = passphrase {
        remember_passphrase(pp);
        save_passphrase_to_disk(pp);
    }
    Ok(())
}

/// Map a common Linux/BSD errno string found in the daemon log tail to a
/// one-line user hint. Returns either `""` (no recognized error) or a
/// hint prefixed with a blank line, suitable for string concat into an
/// error message.
fn diagnose_daemon_exit(log: &str, mode: ServeMode) -> &'static str {
    if log.contains("EADDRNOTAVAIL") || log.contains("Cannot assign requested address") {
        return match mode {
            ServeMode::Local => {
                "\n\nHint: the interface we tried to bind on went away. \
                 Is Tailscale still up?"
            }
            ServeMode::Tunnel => "",
        };
    }
    if log.contains("EADDRINUSE") || log.contains("Address already in use") {
        return "\n\nHint: the daemon couldn't bind the picked port. \
                Reopen the dialog to try again with a fresh random port.";
    }
    if log.contains("Permission denied") {
        return "\n\nHint: permission denied on bind. Are you trying a \
                privileged port (<1024)? We normally pick a high port.";
    }
    ""
}

fn stop_daemon() -> Result<(), String> {
    use std::process::Command;

    let exe =
        std::env::current_exe().map_err(|e| format!("Could not resolve aoe binary path: {}", e))?;

    let output = Command::new(&exe)
        .args(["serve", "--stop"])
        .output()
        .map_err(|e| format!("Failed to invoke `aoe serve --stop`: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(stderr.trim().to_string());
    }
    // Only clear the in-memory cache and ephemeral file. The durable
    // serve.saved_passphrase intentionally survives so the same
    // passphrase is reused on the next launch.
    forget_passphrase();
    Ok(())
}

/// Stop the running daemon and immediately respawn it with the given
/// configuration. Used by passphrase-edit and force-logout flows.
/// Returns `Ok(())` on successful respawn, `Err` if either phase fails.
fn restart_daemon(
    mode: ServeMode,
    passphrase: Option<&str>,
    transport: Option<TunnelTransport>,
) -> Result<(), String> {
    stop_daemon()?;
    spawn_daemon(mode, passphrase, transport)
}

/// Read the current daemon's mode marker (`serve.mode`). Returns None
/// when the file is absent (pre-mode-split daemon) or unparseable.
fn read_serve_mode() -> Option<ServeMode> {
    let dir = crate::session::get_app_dir().ok()?;
    let raw = std::fs::read_to_string(dir.join("serve.mode")).ok()?;
    ServeMode::from_file_token(&raw)
}

/// Read the last mode the user picked (across TUI restarts). Used to
/// default the ModePicker highlight on subsequent opens. Stored in a
/// separate file from `serve.mode` so it survives `aoe serve --stop`.
fn read_last_mode() -> Option<ServeMode> {
    let dir = crate::session::get_app_dir().ok()?;
    let raw = std::fs::read_to_string(dir.join("serve.last_mode")).ok()?;
    ServeMode::from_file_token(&raw)
}

fn remember_last_mode(mode: ServeMode) {
    if let Ok(dir) = crate::session::get_app_dir() {
        let _ = std::fs::write(dir.join("serve.last_mode"), mode.file_token());
    }
}

/// Load a previously used port from `serve.last_port`, or generate a fresh
/// random one in the ephemeral range and persist it. This keeps the URL
/// stable across TUI daemon restarts so users can bookmark it.
fn load_or_generate_port() -> u16 {
    if let Ok(dir) = crate::session::get_app_dir() {
        let port_path = dir.join("serve.last_port");
        if let Ok(raw) = std::fs::read_to_string(&port_path) {
            if let Ok(port) = raw.trim().parse::<u16>() {
                if port >= 49152 {
                    return port;
                }
            }
        }
        // No valid persisted port; generate and save one.
        let port: u16 = rand::rng().random_range(49152..65535);
        let _ = std::fs::write(&port_path, port.to_string());
        return port;
    }
    // Can't access app dir; fall back to random (won't persist).
    rand::rng().random_range(49152..65535)
}

fn log_file_path() -> Option<PathBuf> {
    crate::cli::serve::stdio_redirect_path().ok()
}

fn log_file_size() -> u64 {
    log_file_path()
        .and_then(|p| std::fs::metadata(&p).ok())
        .map(|m| m.len())
        .unwrap_or(0)
}

fn initial_log_tail() -> Vec<String> {
    let Some(path) = log_file_path() else {
        return Vec::new();
    };
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let all: Vec<&str> = contents.lines().collect();
    // debug.log carries TUI + runner + daemon lines now that serve.log is
    // gone. Anchor the initial tail at the last [AOE_START_MARKER] (written
    // by `init_subscriber` for every process) so we show the current
    // daemon's run rather than mixed history. Falls back to the trailing
    // window when no marker is found.
    let anchor = all
        .iter()
        .rposition(|line| line.contains("[AOE_START_MARKER]"))
        .unwrap_or_else(|| all.len().saturating_sub(LOG_TAIL_LINES));
    let from = anchor.min(all.len());
    let window = &all[from..];
    let start = window.len().saturating_sub(LOG_TAIL_LINES);
    window[start..].iter().map(|s| s.to_string()).collect()
}

/// Read any new bytes appended to the log file since `offset` and push the
/// resulting lines into `tail`, clamped to LOG_TAIL_LINES. Returns true if
/// new content arrived.
fn append_new_log_lines(tail: &mut Vec<String>, offset: &mut u64) -> bool {
    let Some(path) = log_file_path() else {
        return false;
    };
    append_new_log_lines_from(&path, tail, offset)
}

/// Path-explicit inner helper so tests can exercise the real logic
/// against a tempfile.
fn append_new_log_lines_from(
    path: &std::path::Path,
    tail: &mut Vec<String>,
    offset: &mut u64,
) -> bool {
    use std::io::{Read, Seek, SeekFrom};

    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let Ok(size) = file.metadata().map(|m| m.len()) else {
        return false;
    };
    if size <= *offset {
        if size < *offset {
            // File was truncated (daemon restart). Reset.
            *offset = 0;
            tail.clear();
        } else {
            return false;
        }
    }

    if file.seek(SeekFrom::Start(*offset)).is_err() {
        return false;
    }
    let mut buf = String::new();
    if file.read_to_string(&mut buf).is_err() {
        return false;
    }
    *offset = size;

    let mut changed = false;
    for line in buf.lines() {
        tail.push(line.to_string());
        changed = true;
    }
    if tail.len() > LOG_TAIL_LINES {
        let drop = tail.len() - LOG_TAIL_LINES;
        tail.drain(..drop);
    }
    changed
}

#[allow(clippy::too_many_arguments)]
fn render_mode_picker(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    selected: ServeMode,
    tunnel_available: bool,
    local_available: bool,
    flash: Option<&str>,
) {
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.accent))
        .title(Line::styled(
            " Remote Access ",
            Style::default().fg(theme.accent).bold(),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Center the content vertically within the full page
    let content_height: u16 = 13; // question + spacer + cards(7) + spacer + flash + keybinds
    let v_pad = inner.height.saturating_sub(content_height) / 2;
    let centered = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(v_pad),
            Constraint::Length(content_height),
            Constraint::Min(0),
        ])
        .split(inner);

    // Constrain card width to avoid stretching across huge terminals
    let max_card_width: u16 = 72;
    let h_pad = centered[1].width.saturating_sub(max_card_width) / 2;
    let h_centered = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(h_pad),
            Constraint::Length(max_card_width.min(centered[1].width)),
            Constraint::Min(0),
        ])
        .split(centered[1]);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // question
            Constraint::Length(1), // spacer
            Constraint::Min(7),    // cards
            Constraint::Length(1), // flash
            Constraint::Length(1), // keybinds
        ])
        .split(h_centered[1]);

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "How should this be reachable?",
            Style::default().fg(theme.title).bold(),
        )))
        .alignment(Alignment::Center),
        rows[0],
    );

    let cards = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(50),
            Constraint::Length(1),
            Constraint::Percentage(50),
        ])
        .split(rows[2]);

    // ── Local card ────────────────────────────────────────────────────────
    let local_primary = crate::server::discover_tagged_ips()
        .into_iter()
        .next()
        .map(|(kind, ip)| match kind {
            crate::server::IpKind::Tailscale => format!("{} (Tailscale)", ip),
            crate::server::IpKind::Lan => format!("{} (LAN)", ip),
            crate::server::IpKind::Loopback => format!("{} (loopback)", ip),
        })
        .unwrap_or_else(|| "only localhost available".to_string());
    let (local_border, local_title_style, local_body_style) =
        if selected == ServeMode::Local && local_available {
            (theme.accent, theme.accent, theme.text)
        } else if !local_available {
            (theme.dimmed, theme.dimmed, theme.dimmed)
        } else {
            (theme.border, theme.title, theme.text)
        };
    let local_block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(local_border))
        .padding(Padding::horizontal(1))
        .title(Line::styled(
            " Local network ",
            Style::default().fg(local_title_style).bold(),
        ));
    let local_inner = local_block.inner(cards[0]);
    frame.render_widget(local_block, cards[0]);
    let local_body = vec![
        Line::from(""),
        Line::from(Span::styled(
            local_primary,
            Style::default().fg(if local_available {
                theme.accent
            } else {
                theme.dimmed
            }),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "Token auth, no passphrase.",
            Style::default().fg(local_body_style),
        )),
        Line::from(Span::styled(
            "LAN + Tailscale. Instant.",
            Style::default().fg(local_body_style),
        )),
        if !local_available {
            Line::from(Span::styled(
                "  (no non-loopback interface)",
                Style::default().fg(theme.dimmed),
            ))
        } else {
            Line::from("")
        },
    ];
    frame.render_widget(Paragraph::new(local_body), local_inner);

    // ── Tunnel card ───────────────────────────────────────────────────────
    let (tunnel_border, tunnel_title_style, tunnel_body_style) =
        if selected == ServeMode::Tunnel && tunnel_available {
            (theme.accent, theme.accent, theme.text)
        } else if !tunnel_available {
            (theme.dimmed, theme.dimmed, theme.dimmed)
        } else {
            (theme.border, theme.title, theme.text)
        };
    let tunnel_block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(tunnel_border))
        .padding(Padding::horizontal(1))
        .title(Line::styled(
            " Internet (HTTPS) ",
            Style::default().fg(tunnel_title_style).bold(),
        ));
    let tunnel_inner = tunnel_block.inner(cards[2]);
    frame.render_widget(tunnel_block, cards[2]);
    let status_line = if tunnel_available {
        "reachable from your phone"
    } else {
        "no tunnel tool installed"
    };
    let tunnel_body = vec![
        Line::from(""),
        Line::from(Span::styled(
            status_line,
            Style::default().fg(if tunnel_available {
                theme.accent
            } else {
                theme.dimmed
            }),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "Token + passphrase (2FA).",
            Style::default().fg(tunnel_body_style),
        )),
        Line::from(Span::styled(
            "Pick transport on next screen.",
            Style::default().fg(tunnel_body_style),
        )),
        if !tunnel_available {
            Line::from(Span::styled(
                "  (brew install tailscale or cloudflared)",
                Style::default().fg(theme.dimmed),
            ))
        } else {
            Line::from("")
        },
    ];
    frame.render_widget(Paragraph::new(tunnel_body), tunnel_inner);

    // ── Flash line ────────────────────────────────────────────────────────
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            flash.unwrap_or(""),
            Style::default().fg(theme.error).bold(),
        )))
        .alignment(Alignment::Center),
        rows[3],
    );

    // ── Keybinds ──────────────────────────────────────────────────────────
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "[←/→] choose    [L] Local    [T] Tunnel    [Enter] confirm    [Esc] cancel",
            Style::default().fg(theme.dimmed),
        )))
        .alignment(Alignment::Center),
        rows[4],
    );
}

fn render_confirm(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    selected: TunnelTransport,
    tailscale: TransportStatus,
    cloudflare: TransportStatus,
    flash: Option<&str>,
) {
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.accent))
        .title(Line::styled(
            " Expose to Internet? ",
            Style::default().fg(theme.accent).bold(),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Center content vertically and constrain width
    let content_height: u16 = 19; // risk(6) + picker(1) + cards(8) + flash + keybinds + margins
    let v_pad = inner.height.saturating_sub(content_height) / 2;
    let centered = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(v_pad),
            Constraint::Length(content_height),
            Constraint::Min(0),
        ])
        .split(inner);

    let max_w: u16 = 82;
    let h_pad = centered[1].width.saturating_sub(max_w) / 2;
    let h_centered = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(h_pad),
            Constraint::Length(max_w.min(centered[1].width)),
            Constraint::Min(0),
        ])
        .split(centered[1]);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(6), // risk explanation
            Constraint::Length(1), // "Pick a transport:"
            Constraint::Min(8),    // cards
            Constraint::Length(1), // flash
            Constraint::Length(1), // keybinds
        ])
        .split(h_centered[1]);

    // ── Risk explanation (compressed; picker below carries most of UI) ───
    let risk = vec![
        Line::from(Span::styled(
            "Your sessions become reachable from anywhere over HTTPS.",
            Style::default().fg(theme.text),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "Two factors required to log in:",
            Style::default().fg(theme.title).bold(),
        )),
        Line::from(vec![
            Span::styled("  \u{2022} ", Style::default().fg(theme.running)),
            Span::styled(
                "token (in the URL / QR code)",
                Style::default().fg(theme.text),
            ),
        ]),
        Line::from(vec![
            Span::styled("  \u{2022} ", Style::default().fg(theme.running)),
            Span::styled(
                "passphrase (typed on the login page)",
                Style::default().fg(theme.text),
            ),
        ]),
        Line::from(Span::styled(
            "Don't share screenshots with BOTH. Stop with [S] when done.",
            Style::default().fg(theme.dimmed),
        )),
    ];
    frame.render_widget(Paragraph::new(risk), rows[0]);

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "Pick a transport:",
            Style::default().fg(theme.title).bold(),
        ))),
        rows[1],
    );

    // ── Transport cards ──────────────────────────────────────────────────
    let cards = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(50),
            Constraint::Length(1),
            Constraint::Percentage(50),
        ])
        .split(rows[2]);

    render_transport_card(
        frame,
        cards[0],
        theme,
        "Tailscale Funnel",
        &[
            "Stable URL across restarts",
            "PWA-friendly on phones",
            "https://<host>.<tailnet>.ts.net",
        ],
        tailscale,
        selected == TunnelTransport::Tailscale,
        /*is_tailscale=*/ true,
    );
    render_transport_card(
        frame,
        cards[2],
        theme,
        "Cloudflare Tunnel",
        &[
            "Works anywhere",
            "URL rotates each restart",
            "Not PWA-friendly",
        ],
        cloudflare,
        selected == TunnelTransport::Cloudflare,
        /*is_tailscale=*/ false,
    );

    // ── Flash ────────────────────────────────────────────────────────────
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            flash.unwrap_or(""),
            Style::default().fg(theme.error).bold(),
        )))
        .alignment(Alignment::Center),
        rows[3],
    );

    // ── Keybinds ─────────────────────────────────────────────────────────
    let keybinds =
        "[←/→] select  [T] Tailscale  [C] Cloudflare  [R] refresh  [Enter] confirm  [Esc] cancel";
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            keybinds,
            Style::default().fg(theme.dimmed),
        )))
        .alignment(Alignment::Center),
        rows[4],
    );
}

#[allow(clippy::too_many_arguments)]
fn render_transport_card(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    title: &str,
    body_lines: &[&str],
    status: TransportStatus,
    is_selected: bool,
    is_tailscale: bool,
) {
    let ready = status.is_ready();
    let (border, title_color, body_color) = if is_selected && ready {
        (theme.accent, theme.accent, theme.text)
    } else if !ready {
        (theme.dimmed, theme.dimmed, theme.dimmed)
    } else {
        (theme.border, theme.title, theme.text)
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border))
        .padding(Padding::horizontal(1))
        .title(Line::styled(
            format!(" {title} "),
            Style::default().fg(title_color).bold(),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines = vec![Line::from("")];
    for text in body_lines {
        lines.push(Line::from(Span::styled(
            *text,
            Style::default().fg(body_color),
        )));
    }
    lines.push(Line::from(""));

    let (status_icon, status_text, status_style) = match status {
        TransportStatus::Ready => (
            "\u{2713}",
            "Ready".to_string(),
            Style::default().fg(theme.running).bold(),
        ),
        TransportStatus::NotInstalled => (
            "\u{26A0}",
            if is_tailscale {
                "Not installed (tailscale up)".to_string()
            } else {
                "Not installed (brew install cloudflared)".to_string()
            },
            Style::default().fg(theme.dimmed),
        ),
        TransportStatus::FunnelNotEnabled => (
            "\u{26A0}",
            "Funnel not enabled for this node".to_string(),
            Style::default().fg(theme.error).bold(),
        ),
    };
    lines.push(Line::from(vec![
        Span::styled(format!("{status_icon} "), status_style),
        Span::styled(status_text, status_style),
    ]));

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
}

fn render_starting(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    mode: ServeMode,
    elapsed: Duration,
) {
    frame.render_widget(Clear, area);
    let (title, wait_line1, wait_line2) = match mode {
        ServeMode::Tunnel => (
            " Starting HTTPS tunnel... ",
            "Waiting for the daemon to bring the tunnel up",
            "(first-time Tailscale cert provisioning can take 30\u{2013}60s).",
        ),
        ServeMode::Local => (
            " Starting local server... ",
            "Binding on 0.0.0.0 and discovering interfaces",
            "(usually under a second).",
        ),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.border))
        .title(Line::styled(title, Style::default().fg(theme.title).bold()));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Center the wait banner vertically
    let content_height: u16 = 5;
    let v_pad = inner.height.saturating_sub(content_height) / 2;
    let centered = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(v_pad),
            Constraint::Length(content_height),
            Constraint::Min(0),
        ])
        .split(inner);

    let banner = vec![
        Line::from(""),
        Line::from(Span::styled(wait_line1, Style::default().fg(theme.text))),
        Line::from(Span::styled(wait_line2, Style::default().fg(theme.text))),
        Line::from(""),
        Line::from(Span::styled(
            format!("Elapsed: {}s    [Esc close]  [S stop]", elapsed.as_secs()),
            Style::default().fg(theme.dimmed),
        )),
    ];
    frame.render_widget(
        Paragraph::new(banner).alignment(Alignment::Center),
        centered[1],
    );
}

/// Shorten a tracing-formatted log line for the in-dialog tail pane.
///
/// Typical input:
///   `2026-04-19T23:43:44.609396Z  INFO agent_of_empires::server::tunnel: Warning: ...`
///
/// Output:
///   `INFO tunnel: Warning: ...`
///
/// Strips the ISO timestamp (the user can see the log is live), compresses
/// the fully-qualified module path down to its last segment, and keeps the
/// level so the user still sees WARN/ERROR when they matter. Leaves
/// non-tracing lines (e.g. stray stdout from `tailscale funnel`) untouched.
fn compact_log_line(raw: &str) -> String {
    let trimmed = raw.trim_end_matches('\n');
    // Detect the tracing prefix: "<ISO8601Z>  LEVEL module::path: message".
    // Require a YYYY-MM-DD-looking prefix rather than "first char is a
    // digit", so stray lines like "200 OK ..." pass through verbatim
    // instead of getting mis-parsed.
    if !looks_like_iso_year(trimmed) {
        return trimmed.to_string();
    }
    // Split off the timestamp (up to the first space after 'Z ').
    let rest = match trimmed.split_once("Z ") {
        Some((_, r)) => r.trim_start(),
        None => return trimmed.to_string(),
    };
    // Split level from the module::path: message remainder.
    let Some((level, after_level)) = rest.split_once(' ') else {
        return trimmed.to_string();
    };
    let after_level = after_level.trim_start();
    // Split "module::path: message" at the ": " that separates path from msg.
    let (path, message) = match after_level.split_once(": ") {
        Some((p, m)) => (p, m),
        None => return format!("{level} {after_level}"),
    };
    let short_path = path.rsplit("::").next().unwrap_or(path);
    format!("{level} {short_path}: {message}")
}

/// Does `s` start with `YYYY-MM-DD`? Fast path for tracing-formatted
/// lines without pulling in a full datetime parser.
fn looks_like_iso_year(s: &str) -> bool {
    let mut iter = s.chars();
    for _ in 0..4 {
        if !iter.next().is_some_and(|c| c.is_ascii_digit()) {
            return false;
        }
    }
    matches!(iter.next(), Some('-'))
}

#[allow(clippy::too_many_arguments)]
fn render_active(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    mode: ServeMode,
    urls: &[ServeUrl],
    url_index: usize,
    passphrase: Option<&str>,
    elapsed: Duration,
    pending_confirm: Option<PendingConfirm>,
) {
    let Some(active_url) = urls.get(url_index).or_else(|| urls.first()) else {
        let msg = "Daemon started but no URL available yet.";
        render_error(frame, area, theme, msg);
        return;
    };
    let url = &active_url.url;
    let kind_label = active_url.label.as_deref();

    let qr_text = match QrCode::new(url.as_bytes()) {
        Ok(code) => code
            .render::<Dense1x2>()
            .quiet_zone(true)
            .dark_color(Dense1x2::Dark)
            .light_color(Dense1x2::Light)
            .build(),
        Err(_) => String::from("(QR unavailable; use the URL below)"),
    };

    let qr_lines: Vec<&str> = qr_text.lines().collect();
    let qr_height = qr_lines.len() as u16;

    let full_url = url.as_str();
    let url_prefix = "URL: ";
    let full_url_len = url_prefix.chars().count() + full_url.chars().count();
    let (split_url, split_token) = split_url_and_token(full_url);

    // Full-page layout: header / content / footer, matching Settings/Diff.
    frame.render_widget(Clear, area);

    let page = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // header
            Constraint::Min(10),   // content
            Constraint::Length(3), // footer
        ])
        .split(area);

    // ── Header ───────────────────────────────────────────────────────────
    let eight_hours = Duration::from_secs(8 * 3600);
    let title_color = if elapsed >= eight_hours {
        theme.waiting
    } else {
        theme.title
    };
    let header_block = Block::default()
        .borders(Borders::BOTTOM)
        .border_style(Style::default().fg(theme.border));
    let header_inner = header_block.inner(page[0]);
    frame.render_widget(header_block, page[0]);

    let mode_label = match mode {
        ServeMode::Local => "local",
        ServeMode::Tunnel => "tunnel",
    };
    let mut header_spans = vec![
        Span::styled(
            format!(" Remote Access ({mode_label})"),
            Style::default().fg(title_color).bold(),
        ),
        Span::styled(
            format!("  open {}", format_elapsed(elapsed)),
            Style::default().fg(theme.dimmed),
        ),
    ];
    if elapsed >= eight_hours {
        header_spans.push(Span::styled(
            "  still need it?",
            Style::default().fg(theme.waiting),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(header_spans)), header_inner);

    // ── Content ──────────────────────────────────────────────────────────
    let content_area = page[1];
    let url_inner_width = content_area.width.saturating_sub(2).max(1) as usize;
    let url_fits_one_line = full_url_len <= url_inner_width;

    let show_passphrase = matches!(mode, ServeMode::Tunnel);
    let show_kind_label = kind_label.is_some();
    let show_split_token = !url_fits_one_line && split_token.is_some();

    // Calculate total content height for vertical centering.
    let mut inner_height: u16 = qr_height + 1 /* spacer */ + 1 /* url */;
    if show_kind_label {
        inner_height += 1;
    }
    if show_split_token {
        inner_height += 1;
    }
    if show_passphrase {
        inner_height += 1;
    }

    let v_pad = content_area.height.saturating_sub(inner_height) / 2;

    let mut constraints = vec![Constraint::Length(v_pad)]; // top padding
    constraints.push(Constraint::Length(qr_height));
    constraints.push(Constraint::Length(1)); // spacer after QR
    if show_kind_label {
        constraints.push(Constraint::Length(1));
    }
    constraints.push(Constraint::Length(1)); // url
    if show_split_token {
        constraints.push(Constraint::Length(1));
    }
    if show_passphrase {
        constraints.push(Constraint::Length(1));
    }
    constraints.push(Constraint::Min(0)); // bottom padding
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .horizontal_margin(1)
        .constraints(constraints)
        .split(content_area);

    // Skip the top padding chunk
    let mut idx: usize = 0;
    idx += 1; // top padding

    // QR code
    let qr_widget: Vec<Line> = qr_lines
        .iter()
        .map(|l| Line::from(Span::styled(*l, Style::default().fg(theme.text))))
        .collect();
    frame.render_widget(
        Paragraph::new(qr_widget).alignment(Alignment::Center),
        chunks[idx],
    );
    idx += 1;

    // Spacer after QR
    idx += 1;

    if let Some(label) = kind_label {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("via {}", label),
                Style::default().fg(theme.dimmed).italic(),
            )))
            .alignment(Alignment::Center),
            chunks[idx],
        );
        idx += 1;
    }

    // URL row(s)
    if url_fits_one_line {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(url_prefix, Style::default().fg(theme.dimmed)),
                Span::styled(full_url, Style::default().fg(theme.accent)),
            ]))
            .alignment(Alignment::Center),
            chunks[idx],
        );
        idx += 1;
    } else {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(url_prefix, Style::default().fg(theme.dimmed)),
                Span::styled(split_url.as_str(), Style::default().fg(theme.accent)),
            ]))
            .alignment(Alignment::Center),
            chunks[idx],
        );
        idx += 1;
        if let Some(token) = split_token {
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("Token: ", Style::default().fg(theme.dimmed)),
                    Span::styled(token, Style::default().fg(theme.accent)),
                ]))
                .alignment(Alignment::Center),
                chunks[idx],
            );
            idx += 1;
        }
    }

    // Passphrase row (Tunnel only)
    if show_passphrase {
        let (pp_label, pp_style) = match passphrase {
            Some(pp) => (pp.to_string(), Style::default().fg(theme.accent).bold()),
            None => (
                "(set when the daemon started; check the shell that ran `aoe serve`)".to_string(),
                Style::default().fg(theme.dimmed),
            ),
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("Passphrase: ", Style::default().fg(theme.dimmed)),
                Span::styled(pp_label, pp_style),
            ]))
            .alignment(Alignment::Center),
            chunks[idx],
        );
    }

    // ── Footer ────────────────────────────────────────────────────────
    let footer_block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(theme.border));
    let footer_inner = footer_block.inner(page[2]);
    frame.render_widget(footer_block, page[2]);

    let key_style = Style::default().fg(theme.accent);
    let desc_style = Style::default().fg(theme.dimmed);

    let footer_line: Line = if let Some(confirm) = pending_confirm {
        let warn_style = Style::default().fg(theme.waiting).bold();
        match confirm {
            PendingConfirm::NewPassphrase => Line::from(Span::styled(
                "Press G again to confirm new passphrase (clients will need it). Any other key cancels.",
                warn_style,
            )),
            PendingConfirm::Restart => Line::from(Span::styled(
                "Press R again to confirm restart (clears all sessions). Any other key cancels.",
                warn_style,
            )),
        }
    } else {
        let mut spans: Vec<Span> = Vec::new();
        if urls.len() > 1 {
            spans.extend([
                Span::styled("Tab", key_style),
                Span::styled(": URL  ", desc_style),
            ]);
        }
        if matches!(mode, ServeMode::Tunnel) {
            spans.extend([
                Span::styled("G", key_style),
                Span::styled(": new pass  ", desc_style),
            ]);
        }
        spans.extend([
            Span::styled("R", key_style),
            Span::styled(": restart  ", desc_style),
            Span::styled("S", key_style),
            Span::styled(": stop  ", desc_style),
            Span::styled("?", key_style),
            Span::styled(": help  ", desc_style),
            Span::styled("Esc", key_style),
            Span::styled(": close", desc_style),
        ]);
        Line::from(spans)
    };
    frame.render_widget(
        Paragraph::new(footer_line).alignment(Alignment::Center),
        footer_inner,
    );
}

fn render_help_overlay(frame: &mut Frame, area: Rect, theme: &Theme, mode: ServeMode) {
    // Size the dialog to fit the longest shortcut description plus
    // the key column (10 chars) plus padding/borders (~6 chars).
    // Clamp to terminal width so narrow terminals still work.
    let dialog_width: u16 = 72.min(area.width.saturating_sub(4));
    let is_tunnel = matches!(mode, ServeMode::Tunnel);
    let dialog_height: u16 = if is_tunnel { 20 } else { 14 };
    let dialog_height = dialog_height.min(area.height.saturating_sub(4));
    let x = area.x + (area.width.saturating_sub(dialog_width)) / 2;
    let y = area.y + (area.height.saturating_sub(dialog_height)) / 2;
    let dialog_area = Rect {
        x,
        y,
        width: dialog_width,
        height: dialog_height,
    };

    frame.render_widget(Clear, dialog_area);
    let block = Block::default()
        .style(Style::default().bg(theme.background))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.border))
        .title(" Remote Access Help ")
        .title_style(
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        );
    let inner = block.inner(dialog_area);
    frame.render_widget(block, dialog_area);

    let mut shortcuts: Vec<(&str, &str)> = Vec::new();
    if is_tunnel {
        shortcuts.push(("G", "New random passphrase and restart server"));
    }
    shortcuts.extend([
        ("R", "Restart server (clears all client sessions)"),
        ("S", "Stop server, return to mode picker"),
        ("Tab", "Cycle URLs (when multiple available)"),
        ("Esc / q", "Close this view (server keeps running)"),
        ("?", "Toggle this help"),
    ]);

    let mut lines: Vec<Line> = vec![
        Line::from(""),
        Line::from(Span::styled(
            "Keyboard Shortcuts",
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];
    for (key, desc) in &shortcuts {
        lines.push(Line::from(vec![
            Span::styled(format!("  {:14}", key), Style::default().fg(theme.waiting)),
            Span::styled(*desc, Style::default().fg(theme.text)),
        ]));
    }
    lines.push(Line::from(""));
    if is_tunnel {
        lines.extend([
            Line::from(Span::styled(
                "About the passphrase",
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                "  Second factor for internet-exposed tunnels.",
                Style::default().fg(theme.text),
            )),
            Line::from(Span::styled(
                "  Persists across stop/start. Press G to rotate.",
                Style::default().fg(theme.text),
            )),
        ]);
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  Press any key to close",
        Style::default().fg(theme.dimmed),
    )));

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

/// Split a URL of the form `https://host/?token=XYZ` into a "clean" base
/// URL and its token so the dialog can fall back to rendering them on
/// separate rows when the combined string would clip off the right edge
/// of the dialog. Returns `(url, None)` when the query param is missing
/// or empty.
fn split_url_and_token(url: &str) -> (String, Option<&str>) {
    // The server always emits the token as the first query param in
    // `{url}/?token={token}`, so `?token=` is a safe anchor.
    if let Some(q_start) = url.find("?token=") {
        let base = url[..q_start].trim_end_matches('?').to_string();
        let token_start = q_start + "?token=".len();
        // Stop at the next `&` in case other query params ever appear.
        let token_end = url[token_start..]
            .find('&')
            .map(|n| token_start + n)
            .unwrap_or(url.len());
        let token = &url[token_start..token_end];
        if !token.is_empty() {
            return (base, Some(token));
        }
    }
    (url.to_string(), None)
}

fn render_error(frame: &mut Frame, area: Rect, theme: &Theme, msg: &str) {
    // Error copy can be long (multi-line tailscale output, stacked log
    // tail, hints, plus remediation steps). Keep it wide + tall enough
    // that the whole message fits without clipping the bottom. Wrap is
    // still on for individual long lines.
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.error))
        .title(Line::styled(
            " Serve failed ",
            Style::default().fg(theme.error).bold(),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);

    frame.render_widget(
        Paragraph::new(msg)
            .wrap(Wrap { trim: true })
            .style(Style::default().fg(theme.text)),
        chunks[0],
    );
    let keybinds = if error_mentions_tailscale(msg) {
        "[S] Force-stop daemon    [R] Reset tailscale funnel    [Enter] Close"
    } else {
        "[S] Force-stop daemon    [Enter] Close"
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            keybinds,
            Style::default().fg(theme.dimmed),
        )))
        .alignment(Alignment::Center),
        chunks[1],
    );
}

/// Heuristic: does this error message relate to a tailscale/funnel issue
/// that `tailscale funnel reset` could plausibly unstick? Used to decide
/// whether to offer the [R] reset keybind on the Error dialog.
fn error_mentions_tailscale(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    lower.contains("tailscale") || lower.contains("funnel")
}

/// Run `tailscale funnel reset` synchronously from the TUI thread.
/// Returns a short error string on failure so the Error dialog can show it.
fn run_tailscale_funnel_reset() -> Result<(), String> {
    let output = std::process::Command::new("tailscale")
        .args(["funnel", "reset"])
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("could not spawn tailscale: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(if stderr.is_empty() {
            format!("exited with status {:?}", output.status.code())
        } else {
            stderr
        })
    }
}

fn format_elapsed(d: Duration) -> String {
    let total = d.as_secs();
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    if h > 0 {
        format!("{}h {:02}m", h, m)
    } else if m > 0 {
        format!("{}m {:02}s", m, s)
    } else {
        format!("{}s", s)
    }
}

/// Generate a four-word lowercase passphrase (1Password / diceware style).
/// Four words from a ~500-word list gives ~35 bits of entropy, which as a
/// *second* factor on top of the URL token is plenty; far easier to type
/// on a phone keyboard than a random alphanumeric soup.
fn generate_passphrase() -> String {
    let mut rng = rand::rng();
    let words: Vec<&'static str> = (0..4)
        .map(|_| {
            *PASSPHRASE_WORDS
                .choose(&mut rng)
                .expect("wordlist nonempty")
        })
        .collect();
    words.join(" ")
}

/// Curated list of short, unambiguous lowercase English words chosen for
/// phone-typability. No words shorter than 3 letters or longer than 6.
/// No near-homophones (e.g., "their"/"there") or visually confusable pairs.
#[rustfmt::skip]
const PASSPHRASE_WORDS: &[&str] = &[
    "able", "acid", "aged", "acorn", "agent", "alarm", "album", "alert",
    "algae", "alien", "alive", "alley", "alloy", "alpha", "amber", "amigo",
    "amino", "amuse", "angel", "anger", "angle", "angry", "ankle", "anvil",
    "apple", "apron", "arbor", "arena", "argon", "armor", "arrow", "ashen",
    "aside", "aspen", "asset", "atlas", "atom", "audio", "audit", "aunt",
    "avoid", "awake", "award", "aware", "awful", "axis", "bacon", "badge",
    "bagel", "baker", "balmy", "banjo", "baron", "basil", "basin", "basis",
    "batch", "baton", "beach", "beads", "beard", "beast", "beaver", "bench",
    "berry", "bingo", "birch", "bison", "black", "blade", "blaze", "blend",
    "bliss", "block", "bloom", "blues", "blunt", "blush", "board", "boast",
    "bold", "bolt", "bonus", "boost", "booth", "boots", "bored", "boss",
    "botany", "bowl", "brave", "bread", "break", "brick", "bride", "brief",
    "bring", "brisk", "brook", "brown", "brush", "bucket", "bugle", "built",
    "bulk", "bunny", "burly", "butter", "buzz", "cabin", "cable", "cactus",
    "caddy", "camel", "camp", "candle", "candy", "canoe", "canon", "canyon",
    "cape", "caper", "card", "care", "cargo", "carry", "cart", "carve",
    "cash", "cast", "catch", "cedar", "chair", "chalk", "charm", "chart",
    "chase", "cheek", "cheer", "chef", "chess", "chief", "child", "chill",
    "chimp", "chip", "chirp", "choir", "chose", "chunk", "cider", "cinema",
    "civic", "claim", "clamp", "clean", "clerk", "click", "cliff", "climb",
    "cling", "clock", "clone", "cloth", "cloud", "clove", "clown", "club",
    "clue", "coach", "coast", "cobra", "cocoa", "code", "coin", "colon",
    "color", "comet", "coral", "cord", "corn", "cost", "couch", "cover",
    "cozy", "craft", "crane", "crash", "crate", "cream", "crest", "crew",
    "cross", "crowd", "crown", "crumb", "crush", "crust", "cube", "curl",
    "cycle", "daisy", "dance", "dare", "dash", "data", "deal", "deck",
    "delta", "dense", "depth", "derby", "desk", "diary", "dice", "diner",
    "disco", "diver", "dock", "dodo", "dog", "doll", "dolly", "donkey",
    "dough", "dove", "downy", "draft", "dragon", "drape", "dream", "drift",
    "drill", "drive", "drop", "drum", "duck", "dusk", "dusty", "eager",
    "eagle", "early", "earth", "ebony", "echo", "edge", "eject", "elbow",
    "elder", "elf", "elite", "elk", "elm", "email", "empty", "enact",
    "energy", "engine", "enjoy", "enter", "entry", "envoy", "epic", "equal",
    "era", "error", "essay", "ether", "event", "every", "exact", "exile",
    "exit", "extra", "eye", "fable", "face", "fact", "fade", "fair",
    "fairy", "faith", "fall", "false", "fame", "family", "fancy", "farm",
    "fast", "fat", "fate", "fault", "fawn", "fear", "feast", "feed",
    "fern", "ferry", "fever", "few", "fiber", "field", "fifth", "fig",
    "film", "find", "fine", "finer", "finish", "fire", "firm", "first",
    "fish", "five", "fix", "flag", "flame", "flash", "flat", "flax",
    "flex", "flint", "float", "flock", "flood", "floor", "flora", "flour",
    "flow", "flower", "fluff", "fluid", "fluke", "flute", "fly", "foam",
    "fog", "foil", "fold", "folk", "fond", "food", "foot", "force",
    "ford", "forge", "fork", "form", "fort", "forum", "fossil", "fox",
    "frame", "free", "fresh", "friar", "fries", "frog", "from", "front",
    "frost", "froth", "fruit", "fry", "fuel", "full", "fun", "fund",
    "funny", "fur", "fury", "fuse", "gable", "gadget", "gain", "gala",
    "gamma", "gap", "garden", "gargle", "garlic", "gate", "gauge", "gear",
    "gecko", "gem", "gentle", "gift", "ginger", "girl", "glad", "glide",
    "glitch", "globe", "gloom", "gloss", "glove", "glow", "glue", "gnat",
    "goat", "gold", "golf", "gone", "good", "goose", "gospel", "grab",
    "grace", "grade", "grain", "grape", "graph", "grasp", "grass", "grate",
    "gravy", "great", "grid", "grief", "grim", "grin", "grip", "grit",
    "groan", "groom", "gross", "group", "grout", "grove", "grow", "grub",
    "guess", "guide", "guild", "guilt", "guitar", "gulf", "gum", "guru",
    "habit", "haiku", "hair", "half", "hall", "halt", "ham", "hand",
    "hang", "happy", "harbor", "hard", "hare", "harm", "harp", "hash",
    "haste", "hat", "hatch", "have", "haven", "hawk", "hay", "hazel",
    "head", "heal", "heap", "heart", "heat", "heavy", "hedge", "heel",
    "help", "hemp", "hen", "herb", "hero", "hex", "hide", "high",
    "hike", "hill", "hip", "hive", "hobby", "hog", "hold", "hole",
    "hollow", "holy", "home", "honey", "honor", "hood", "hoof", "hook",
    "hoop", "hope", "horn", "horse", "host", "hot", "hound", "hour",
    "house", "hub", "hug", "human", "humble", "humor", "hump", "hunch",
    "hunt", "hurry", "husk", "hut", "hyena", "hymn", "ice", "icon",
    "idea", "igloo", "imp", "index", "indigo", "infant", "inlet", "ink",
    "inlay", "inner", "input", "iris", "iron", "ivory", "ivy", "jade",
    "jam", "jar", "java", "jaw", "jazz", "jeans", "jelly", "jest",
    "jet", "jewel", "jiffy", "jig", "job", "join", "joke", "jolly",
    "joy", "judge", "juice", "jump", "jungle", "junior", "junk", "jury",
    "kayak", "keep", "kept", "kettle", "key", "kick", "kid", "kilt",
    "kind", "king", "kite", "kitten", "knack", "knee", "knife", "knock",
    "koala", "label", "lace", "ladder", "lake", "lamb", "lamp", "lance",
    "land", "lane", "laser", "later", "latte", "laugh", "lava", "lawn",
    "layer", "lazy", "leaf", "lean", "leap", "learn", "lease", "led",
    "ledge", "left", "legal", "lemon", "lend", "lens", "level", "lever",
    "lick", "lid", "life", "lift", "light", "lilac", "lime", "line",
    "link", "lint", "lion", "lip", "list", "live", "load", "loaf",
    "loan", "lobby", "lobe", "local", "lock", "loft", "log", "logic",
    "long", "look", "loop", "loose", "lotus", "loud", "lounge", "love",
    "low", "loyal", "luck", "lunar", "lunch", "lung", "lure", "lush",
    "lute", "lynx", "lyric", "mace", "madam", "made", "magic", "main",
    "make", "mallet", "malt", "mango", "manor", "mantle", "maple", "march",
    "mare", "mark", "mars", "marsh", "mask", "mast", "match", "mate",
    "math", "maze", "meadow", "meal", "meat", "medal", "meet", "mellow",
    "melody", "melt", "memo", "menu", "mercy", "merge", "merit", "merry",
    "mesh", "metal", "meter", "mew", "mice", "midst", "might", "mild",
    "mile", "milk", "mill", "mimic", "mind", "mine", "mint", "minus",
    "mirror", "mist", "moat", "mocha", "modal", "model", "modem", "moist",
    "mole", "money", "month", "moon", "moose", "moral", "more", "moth",
    "motor", "mount", "mouse", "move", "movie", "much", "muffin", "mulch",
    "mule", "muse", "music", "mute", "myth",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passphrase_is_four_lowercase_words() {
        let pw = generate_passphrase();
        let words: Vec<&str> = pw.split(' ').collect();
        assert_eq!(words.len(), 4, "passphrase should be 4 words: {:?}", pw);
        for w in &words {
            assert!(!w.is_empty(), "empty word in passphrase: {:?}", pw);
            assert!(
                w.chars().all(|c| c.is_ascii_lowercase()),
                "non-lowercase-letter in word {:?} of {:?}",
                w,
                pw
            );
        }
    }

    #[test]
    fn passphrase_words_are_from_the_wordlist() {
        let pw = generate_passphrase();
        for w in pw.split(' ') {
            assert!(
                PASSPHRASE_WORDS.contains(&w),
                "word {:?} not in the embedded wordlist",
                w
            );
        }
    }

    #[test]
    fn compact_log_line_strips_tracing_prefix() {
        let input = "2026-04-19T23:43:44.609396Z  INFO agent_of_empires::server::tunnel: Warning: funnel=on for foo, but no serve config";
        assert_eq!(
            compact_log_line(input),
            "INFO tunnel: Warning: funnel=on for foo, but no serve config"
        );
    }

    #[test]
    fn compact_log_line_preserves_passthrough() {
        // Lines without a tracing-style leading timestamp (e.g. stray
        // stdout from tailscale funnel) should pass through unchanged.
        let raw = "Available on the internet: https://foo.ts.net";
        assert_eq!(compact_log_line(raw), raw);
    }

    #[test]
    fn compact_log_line_handles_levels() {
        let error = "2026-04-19T23:43:44.669741Z ERROR agent_of_empires::server: boom";
        assert_eq!(compact_log_line(error), "ERROR server: boom");
    }

    #[test]
    fn compact_log_line_leaves_digit_prefixed_non_tracing_alone() {
        // Regression: earlier heuristic flagged anything starting with a
        // digit as a tracing line, mangling lines like HTTP status codes.
        let line = "200 OK received";
        assert_eq!(compact_log_line(line), "200 OK received");
    }

    #[test]
    fn wordlist_is_well_formed() {
        assert!(
            PASSPHRASE_WORDS.len() >= 256,
            "wordlist too small for reasonable entropy: {}",
            PASSPHRASE_WORDS.len()
        );
        for w in PASSPHRASE_WORDS {
            assert!(!w.is_empty(), "empty word in list");
            assert!(
                w.chars().all(|c| c.is_ascii_lowercase()),
                "non-lowercase word in list: {:?}",
                w
            );
        }
    }

    #[test]
    fn format_elapsed_shows_units() {
        assert_eq!(format_elapsed(Duration::from_secs(5)), "5s");
        assert_eq!(format_elapsed(Duration::from_secs(65)), "1m 05s");
        assert_eq!(format_elapsed(Duration::from_secs(3600 + 120)), "1h 02m");
    }

    #[test]
    fn append_new_log_lines_initial_read() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("debug.log");
        std::fs::write(&path, "first line\nsecond line\n").unwrap();

        let mut tail: Vec<String> = Vec::new();
        let mut offset: u64 = 0;
        let grew = append_new_log_lines_from(&path, &mut tail, &mut offset);
        assert!(grew);
        assert_eq!(tail, vec!["first line", "second line"]);
        assert_eq!(offset, std::fs::metadata(&path).unwrap().len());
    }

    #[test]
    fn append_new_log_lines_detects_growth_and_truncation() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("debug.log");

        // Seed.
        std::fs::write(&path, "one\ntwo\n").unwrap();
        let mut tail: Vec<String> = Vec::new();
        let mut offset: u64 = 0;
        assert!(append_new_log_lines_from(&path, &mut tail, &mut offset));
        assert_eq!(tail, vec!["one", "two"]);
        let after_seed_offset = offset;

        // Append only.
        std::fs::write(&path, "one\ntwo\nthree\n").unwrap();
        assert!(append_new_log_lines_from(&path, &mut tail, &mut offset));
        assert_eq!(tail, vec!["one", "two", "three"]);
        assert!(offset > after_seed_offset);

        // No growth → no change.
        let before = offset;
        assert!(!append_new_log_lines_from(&path, &mut tail, &mut offset));
        assert_eq!(offset, before);

        // Truncation (daemon restart): file shrank, tail resets.
        std::fs::write(&path, "fresh\n").unwrap();
        assert!(append_new_log_lines_from(&path, &mut tail, &mut offset));
        assert_eq!(tail, vec!["fresh"]);
    }

    #[test]
    fn append_new_log_lines_clamps_to_max_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("debug.log");

        // Write well over LOG_TAIL_LINES.
        let mut big = String::new();
        for i in 0..(LOG_TAIL_LINES + 50) {
            big.push_str(&format!("line {}\n", i));
        }
        std::fs::write(&path, big).unwrap();

        let mut tail: Vec<String> = Vec::new();
        let mut offset: u64 = 0;
        assert!(append_new_log_lines_from(&path, &mut tail, &mut offset));
        assert_eq!(tail.len(), LOG_TAIL_LINES);
        assert_eq!(
            tail.last().unwrap(),
            &format!("line {}", LOG_TAIL_LINES + 49)
        );
    }

    // These tests share the module-global LAST_SPAWNED_PASSPHRASE, so they
    // are combined into one #[test] to avoid cross-test interference when
    // cargo runs them in parallel. Uses the in-memory helpers so we don't
    // touch the user's real serve.passphrase file during `cargo test`.
    #[test]
    fn passphrase_cache_roundtrip() {
        forget_passphrase_in_memory();
        assert_eq!(recall_passphrase_in_memory(), None);

        remember_passphrase("four word diceware phrase");
        assert_eq!(
            recall_passphrase_in_memory().as_deref(),
            Some("four word diceware phrase")
        );

        remember_passphrase("a different phrase later");
        assert_eq!(
            recall_passphrase_in_memory().as_deref(),
            Some("a different phrase later")
        );

        forget_passphrase_in_memory();
        assert_eq!(recall_passphrase_in_memory(), None);
    }

    #[test]
    fn split_url_and_token_extracts_token() {
        let (base, token) =
            split_url_and_token("https://foo-bar.trycloudflare.com/?token=abc123def456");
        assert_eq!(base, "https://foo-bar.trycloudflare.com/");
        assert_eq!(token, Some("abc123def456"));
    }

    #[test]
    fn split_url_and_token_preserves_url_without_token() {
        let (base, token) = split_url_and_token("https://foo-bar.trycloudflare.com/");
        assert_eq!(base, "https://foo-bar.trycloudflare.com/");
        assert_eq!(token, None);
    }

    #[test]
    fn split_url_and_token_handles_additional_query_params() {
        let (base, token) =
            split_url_and_token("https://foo.trycloudflare.com/?token=abc123&foo=bar");
        assert_eq!(base, "https://foo.trycloudflare.com/");
        assert_eq!(token, Some("abc123"));
    }

    /// Exercises the fit logic that the render path uses: full URL on
    /// one line when it fits, split when it doesn't. Copies the arithmetic
    /// from render_active (url_inner_width = dialog_width - 4).
    fn url_fits_one_line(url: &str, dialog_width: u16) -> bool {
        let url_prefix = "URL: ";
        let full_url_len = url_prefix.chars().count() + url.chars().count();
        let url_inner_width = dialog_width.saturating_sub(4).max(1) as usize;
        full_url_len <= url_inner_width
    }

    #[test]
    fn url_fits_one_line_on_wide_terminal() {
        // Typical tunnel URL: ~115 chars including "URL: " prefix.
        let url = "https://foo-bar.trycloudflare.com/?token=a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2";
        assert!(
            url_fits_one_line(url, 120),
            "120-wide should fit ~115 chars"
        );
        assert!(
            url_fits_one_line(url, 115),
            "exact-fit boundary should pass"
        );
    }

    #[test]
    fn url_splits_on_narrow_terminal() {
        // 80-col terminal can't fit the combined tunnel URL; force the
        // split fallback so the token doesn't clip off the edge.
        let url = "https://foo-bar.trycloudflare.com/?token=a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2";
        assert!(!url_fits_one_line(url, 80));
        // Local URL is shorter (~70 with token) — depends on IP/port.
        let local = "http://192.168.1.42:54321/?token=a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2";
        assert!(!url_fits_one_line(local, 80));
        assert!(url_fits_one_line(local, 110));
    }

    #[test]
    fn serve_mode_file_token_roundtrip() {
        assert_eq!(ServeMode::from_file_token("local"), Some(ServeMode::Local));
        assert_eq!(
            ServeMode::from_file_token("tunnel"),
            Some(ServeMode::Tunnel)
        );
        // Trailing newline (the way the server writes it) still parses.
        assert_eq!(
            ServeMode::from_file_token("local\n"),
            Some(ServeMode::Local)
        );
        assert_eq!(ServeMode::from_file_token("garbage"), None);
        assert_eq!(ServeMode::from_file_token(""), None);
    }

    #[test]
    fn diagnose_daemon_exit_recognizes_common_errnos() {
        // Tailscale drop on Local: EADDRNOTAVAIL
        let hint = diagnose_daemon_exit(
            "ERROR: bind: Cannot assign requested address",
            ServeMode::Local,
        );
        assert!(hint.contains("interface"));
        // Same errno in Tunnel is not actionable in the same way, so we
        // don't surface a hint.
        assert_eq!(
            diagnose_daemon_exit(
                "ERROR: bind: Cannot assign requested address",
                ServeMode::Tunnel,
            ),
            ""
        );
        // Port-in-use
        assert!(diagnose_daemon_exit("Address already in use", ServeMode::Local).contains("port"));
        // Permission denied on privileged port
        assert!(diagnose_daemon_exit("Permission denied", ServeMode::Tunnel).contains("permission"));
        // No match
        assert_eq!(
            diagnose_daemon_exit("some unrelated line", ServeMode::Local),
            ""
        );
    }

    // ── read_serve_urls ───────────────────────────────────────────────────
    //
    // The helper reads from $APP_DIR/serve.url, which is outside our
    // control in unit tests. These tests exercise the parsing logic via a
    // small shim that mirrors read_serve_urls' line-by-line behavior; the
    // integration with the real file lives in e2e.
    fn parse_serve_url_contents(raw: &str) -> Vec<ServeUrl> {
        let mut out: Vec<ServeUrl> = Vec::new();
        for (i, line) in raw.lines().enumerate() {
            let line = line.trim_end_matches('\r');
            if line.is_empty() {
                continue;
            }
            if i == 0 {
                out.push(ServeUrl {
                    label: None,
                    url: line.to_string(),
                });
            } else if let Some((label, url)) = line.split_once('\t') {
                out.push(ServeUrl {
                    label: Some(label.to_string()),
                    url: url.to_string(),
                });
            } else {
                out.push(ServeUrl {
                    label: None,
                    url: line.to_string(),
                });
            }
        }
        out
    }

    #[test]
    fn serve_url_parses_single_line_backward_compat() {
        // Older tunnel daemons wrote only the public URL on line 1.
        let out = parse_serve_url_contents("https://foo.trycloudflare.com/?token=abc\n");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].label, None);
        assert_eq!(out[0].url, "https://foo.trycloudflare.com/?token=abc");
    }

    #[test]
    fn serve_url_parses_multi_line_with_labels() {
        // Current daemons write primary on line 1, `kind\turl` on alternates.
        let raw = "\
http://100.64.0.5:54321/?token=abc\n\
lan\thttp://192.168.1.20:54321/?token=abc\n\
localhost\thttp://localhost:54321/?token=abc\n";
        let out = parse_serve_url_contents(raw);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].label, None);
        assert_eq!(out[0].url, "http://100.64.0.5:54321/?token=abc");
        assert_eq!(out[1].label.as_deref(), Some("lan"));
        assert_eq!(out[1].url, "http://192.168.1.20:54321/?token=abc");
        assert_eq!(out[2].label.as_deref(), Some("localhost"));
    }

    #[test]
    fn serve_url_tolerates_empty_and_unlabeled_extras() {
        // Defensive: if someone hand-edits serve.url and an extra line
        // has no tab, we treat it as an unlabeled alt rather than
        // dropping it.
        let raw = "http://primary/\n\nhttp://no-label-here/\n";
        let out = parse_serve_url_contents(raw);
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].label, None);
        assert_eq!(out[1].url, "http://no-label-here/");
    }

    // ── load_or_generate_port ────────────────────────────────────────────
    //
    // The real function reads from $APP_DIR/serve.last_port, which we can't
    // control in unit tests. These tests exercise the same parse + validate
    // + generate logic via a small shim that mirrors the function's core.

    /// Mirrors load_or_generate_port's logic against an arbitrary directory
    /// so we can test without touching the real app dir.
    fn load_or_generate_port_from(dir: &std::path::Path) -> u16 {
        let port_path = dir.join("serve.last_port");
        if let Ok(raw) = std::fs::read_to_string(&port_path) {
            if let Ok(port) = raw.trim().parse::<u16>() {
                if port >= 49152 {
                    return port;
                }
            }
        }
        let port: u16 = rand::rng().random_range(49152..65535);
        let _ = std::fs::write(&port_path, port.to_string());
        port
    }

    #[test]
    fn load_or_generate_port_generates_and_persists() {
        let tmp = tempfile::tempdir().unwrap();
        let port = load_or_generate_port_from(tmp.path());
        assert!(port >= 49152, "generated port should be in ephemeral range");
        // File was written
        let raw = std::fs::read_to_string(tmp.path().join("serve.last_port")).unwrap();
        assert_eq!(raw, port.to_string());
    }

    #[test]
    fn load_or_generate_port_reuses_persisted() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("serve.last_port"), "55555").unwrap();
        let port = load_or_generate_port_from(tmp.path());
        assert_eq!(port, 55555);
    }

    #[test]
    fn load_or_generate_port_rejects_low_port() {
        let tmp = tempfile::tempdir().unwrap();
        // A port below the ephemeral range should be ignored and regenerated.
        std::fs::write(tmp.path().join("serve.last_port"), "8080").unwrap();
        let port = load_or_generate_port_from(tmp.path());
        assert!(port >= 49152, "low port should be rejected: got {}", port);
    }

    #[test]
    fn load_or_generate_port_handles_garbage_file() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("serve.last_port"), "not-a-number\n").unwrap();
        let port = load_or_generate_port_from(tmp.path());
        assert!(port >= 49152, "garbage content should be regenerated");
    }

    /// The help overlay has a fixed width. Every shortcut line must fit
    /// within `dialog_width - borders(2) - padding(0)` columns so text
    /// doesn't clip. This test catches the bug before it ships.
    #[test]
    fn help_overlay_text_fits_within_dialog_width() {
        let dialog_width: usize = 72;
        // Inner width = dialog_width - 2 (left/right border)
        let inner_width = dialog_width - 2;
        let key_col: usize = 14; // format!("{:14}", key)
        let indent: usize = 2; // leading "  "

        // All possible shortcut descriptions (union of Tunnel + Local)
        let descriptions = [
            "New random passphrase and restart server",
            "Restart server (clears all client sessions)",
            "Stop server, return to mode picker",
            "Cycle URLs (when multiple available)",
            "Close this view (server keeps running)",
            "Toggle this help",
            // Section headers and other lines
            "Keyboard Shortcuts",
            "About the passphrase",
            "Second factor for internet-exposed tunnels.",
            "Persists across stop/start. Press G to rotate.",
            "Press any key to close",
        ];

        for desc in descriptions {
            let line_len = indent + key_col + desc.len();
            assert!(
                line_len <= inner_width,
                "Help text clips: {:?} needs {} cols but only {} available \
                 (dialog_width={}, inner={})",
                desc,
                line_len,
                inner_width,
                dialog_width,
                inner_width,
            );
        }
    }
}
