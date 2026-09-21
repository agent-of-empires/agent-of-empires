//! Remote access view: a controller for the `aoe serve --daemon` lifecycle, which outlives the TUI.

mod daemon;
mod render;
mod words;

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;

use crate::cli::serve::{read_serve_urls, ServeUrl};
use crate::tui::styles::Theme;
pub(crate) use daemon::start_local_daemon_and_wait;
use daemon::*;

const TUNNEL_STARTUP_TIMEOUT_SECS: u64 = 60;
const FLASH_TTL: Duration = Duration::from_millis(1500);
const CONFIRM_TTL: Duration = Duration::from_secs(3);

pub enum ServeAction {
    Continue,
    Close,
}

/// Persisted to `serve.mode` and `serve.last_mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServeMode {
    Local,
    Tunnel,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TunnelTransport {
    Tailscale,
    Cloudflare,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransportStatus {
    Ready,
    NotInstalled,
    /// Tailscale is logged in but the ACL doesn't grant Funnel.
    FunnelNotEnabled,
}

/// Transient message shown after a rejected keypress.
type Flash = Option<(String, Instant)>;

fn expire_flash(flash: &mut Flash) -> bool {
    let expired = flash.as_ref().is_some_and(|(_, t)| t.elapsed() > FLASH_TTL);
    if expired {
        *flash = None;
    }
    expired
}

enum ServeViewState {
    ModePicker {
        selected: ServeMode,
        tunnel_available: bool,
        local_available: bool,
        flash: Flash,
    },
    /// Tunnel only: risk explanation plus the transport picker.
    Confirm {
        selected: TunnelTransport,
        tailscale: TransportStatus,
        cloudflare: TransportStatus,
        flash: Flash,
    },
    /// Polling `serve.url`; `transport` is None for Local and reattached daemons.
    Starting {
        mode: ServeMode,
        transport: Option<TunnelTransport>,
        passphrase: Option<String>,
        started_at: Instant,
        log_offset: u64,
    },
    Active {
        mode: ServeMode,
        transport: Option<TunnelTransport>,
        urls: Vec<ServeUrl>,
        url_index: usize,
        /// Known only when this TUI started the daemon or the server wrote it to disk.
        passphrase: Option<String>,
        opened_at: Instant,
        log_offset: u64,
    },
    Error(String),
}

impl ServeViewState {
    fn mode_picker() -> Self {
        let tunnel_available = crate::server::tunnel::tailscale_available_sync()
            || crate::server::tunnel::check_cloudflared().is_ok();
        let local_available = !crate::server::discover_tagged_ips().is_empty();
        // Prefer the last launched mode, falling back to whichever one is available.
        let last = read_last_mode().unwrap_or(ServeMode::Local);
        let selected = match (last, local_available, tunnel_available) {
            (ServeMode::Local, false, true) | (ServeMode::Tunnel, _, true) => ServeMode::Tunnel,
            _ => ServeMode::Local,
        };
        ServeViewState::ModePicker {
            selected,
            tunnel_available,
            local_available,
            flash: None,
        }
    }

    /// Active once the daemon has published its URLs, Starting until then.
    fn running(
        mode: ServeMode,
        transport: Option<TunnelTransport>,
        passphrase: Option<String>,
        log_offset: u64,
    ) -> Self {
        let urls = read_serve_urls();
        if urls.is_empty() {
            return ServeViewState::Starting {
                mode,
                transport,
                passphrase,
                started_at: Instant::now(),
                log_offset,
            };
        }
        ServeViewState::Active {
            mode,
            transport,
            urls,
            url_index: 0,
            passphrase,
            opened_at: Instant::now(),
            log_offset,
        }
    }
}

/// A destructive action awaiting a second press of its key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingConfirm {
    NewPassphrase,
    Restart,
}

pub struct ServeView {
    state: ServeViewState,
    /// Passphrase for the next Tunnel spawn; persisted so it survives stop/start.
    pending_passphrase: String,
    pending_confirm: Option<(PendingConfirm, Instant)>,
    show_help: bool,
}

impl Default for ServeView {
    fn default() -> Self {
        Self::new()
    }
}

impl ServeView {
    /// Reattaches to a running daemon, otherwise opens the mode picker.
    pub fn new() -> Self {
        let pending_passphrase = load_or_generate_passphrase();
        let state = if crate::cli::serve::daemon_pid().is_some() {
            // Daemons that predate `serve.mode` could only be Tunnel.
            let mode = read_serve_mode().unwrap_or(ServeMode::Tunnel);
            let passphrase = (mode == ServeMode::Tunnel)
                .then(recall_passphrase)
                .flatten();
            ServeViewState::running(mode, None, passphrase, log_file_size())
        } else {
            ServeViewState::mode_picker()
        };
        Self {
            state,
            pending_passphrase,
            pending_confirm: None,
            show_help: false,
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> ServeAction {
        match self.state {
            ServeViewState::ModePicker { .. } => self.mode_picker_key(key.code),
            ServeViewState::Confirm { .. } => self.confirm_key(key.code),
            ServeViewState::Starting { .. } => match key.code {
                // Closing leaves the daemon coming up; `s` aborts it.
                KeyCode::Esc | KeyCode::Char('q') => ServeAction::Close,
                KeyCode::Char('s' | 'S') => {
                    let _ = stop_daemon();
                    ServeAction::Close
                }
                _ => ServeAction::Continue,
            },
            ServeViewState::Active { .. } => self.active_key(key.code),
            ServeViewState::Error(_) => self.error_key(key.code),
        }
    }

    fn mode_picker_key(&mut self, code: KeyCode) -> ServeAction {
        let ServeViewState::ModePicker {
            selected,
            tunnel_available,
            local_available,
            flash,
        } = &mut self.state
        else {
            return ServeAction::Continue;
        };
        expire_flash(flash);
        match code {
            KeyCode::Left | KeyCode::Char('h') => *selected = ServeMode::Local,
            KeyCode::Right | KeyCode::Char('l') if *tunnel_available => {
                *selected = ServeMode::Tunnel
            }
            KeyCode::Tab => {
                *selected = match *selected {
                    ServeMode::Local if *tunnel_available => ServeMode::Tunnel,
                    ServeMode::Tunnel if *local_available => ServeMode::Local,
                    other => other,
                }
            }
            KeyCode::Char('t' | 'T') => {
                *selected = ServeMode::Tunnel;
                return self.commit_mode();
            }
            // Lowercase `l` moves right, so capital `L` is the Local shortcut.
            KeyCode::Char('L') => {
                *selected = ServeMode::Local;
                return self.commit_mode();
            }
            KeyCode::Enter => return self.commit_mode(),
            KeyCode::Esc | KeyCode::Char('q') => return ServeAction::Close,
            _ => {}
        }
        ServeAction::Continue
    }

    fn commit_mode(&mut self) -> ServeAction {
        let ServeViewState::ModePicker {
            selected,
            tunnel_available,
            local_available,
            flash,
        } = &mut self.state
        else {
            return ServeAction::Continue;
        };
        let rejection = match *selected {
            ServeMode::Tunnel if !*tunnel_available => {
                "Install tailscale or cloudflared to enable Tunnel mode."
            }
            ServeMode::Local if !*local_available => "No non-loopback network interface available.",
            ServeMode::Tunnel => {
                let (tailscale, cloudflare) = assess_transports();
                self.state = ServeViewState::Confirm {
                    selected: default_transport(tailscale, cloudflare),
                    tailscale,
                    cloudflare,
                    flash: None,
                };
                return ServeAction::Continue;
            }
            ServeMode::Local => {
                self.spawn(ServeMode::Local, None);
                return ServeAction::Continue;
            }
        };
        *flash = Some((rejection.to_string(), Instant::now()));
        ServeAction::Continue
    }

    fn confirm_key(&mut self, code: KeyCode) -> ServeAction {
        let ServeViewState::Confirm {
            selected,
            tailscale,
            cloudflare,
            flash,
        } = &mut self.state
        else {
            return ServeAction::Continue;
        };
        expire_flash(flash);
        match code {
            KeyCode::Left | KeyCode::Char('h') => *selected = TunnelTransport::Tailscale,
            KeyCode::Right | KeyCode::Char('l') => *selected = TunnelTransport::Cloudflare,
            KeyCode::Tab => {
                *selected = match *selected {
                    TunnelTransport::Tailscale => TunnelTransport::Cloudflare,
                    TunnelTransport::Cloudflare => TunnelTransport::Tailscale,
                }
            }
            KeyCode::Char('t' | 'T') => {
                *selected = TunnelTransport::Tailscale;
                return self.commit_transport();
            }
            KeyCode::Char('c' | 'C') => {
                *selected = TunnelTransport::Cloudflare;
                return self.commit_transport();
            }
            KeyCode::Enter => return self.commit_transport(),
            KeyCode::Char('r' | 'R') => {
                (*tailscale, *cloudflare) = assess_transports();
                *flash = Some(("Refreshed.".to_string(), Instant::now()));
            }
            KeyCode::Esc | KeyCode::Char('q') => return ServeAction::Close,
            _ => {}
        }
        ServeAction::Continue
    }

    fn commit_transport(&mut self) -> ServeAction {
        let ServeViewState::Confirm {
            selected,
            tailscale,
            cloudflare,
            flash,
        } = &mut self.state
        else {
            return ServeAction::Continue;
        };
        let pick = *selected;
        let rejection = match (pick, *tailscale, *cloudflare) {
            (TunnelTransport::Tailscale, TransportStatus::Ready, _)
            | (TunnelTransport::Cloudflare, _, TransportStatus::Ready) => {
                self.spawn(ServeMode::Tunnel, Some(pick));
                return ServeAction::Continue;
            }
            (TunnelTransport::Tailscale, TransportStatus::FunnelNotEnabled, _) => {
                "Tailscale Funnel isn't enabled for this node; pick Cloudflare or update your ACL."
            }
            (TunnelTransport::Tailscale, ..) => "Tailscale isn't installed; pick Cloudflare.",
            (TunnelTransport::Cloudflare, ..) => "cloudflared isn't installed; pick Tailscale.",
        };
        *flash = Some((rejection.to_string(), Instant::now()));
        ServeAction::Continue
    }

    fn spawn(&mut self, mode: ServeMode, transport: Option<TunnelTransport>) {
        // Taken before spawning so only the new daemon's output counts as progress.
        let log_offset = log_file_size();
        let passphrase = (mode == ServeMode::Tunnel).then(|| self.pending_passphrase.clone());
        self.state = match spawn_daemon(mode, passphrase.as_deref(), transport) {
            Ok(()) => {
                remember_last_mode(mode);
                ServeViewState::Starting {
                    mode,
                    transport,
                    passphrase,
                    started_at: Instant::now(),
                    log_offset,
                }
            }
            Err(e) => ServeViewState::Error(e),
        };
    }

    fn active_key(&mut self, code: KeyCode) -> ServeAction {
        if std::mem::take(&mut self.show_help) {
            return ServeAction::Continue;
        }
        let confirmed = self.pending_confirm.take().and_then(|(action, when)| {
            let repeated = match action {
                PendingConfirm::NewPassphrase => matches!(code, KeyCode::Char('g' | 'G')),
                PendingConfirm::Restart => matches!(code, KeyCode::Char('r' | 'R')),
            };
            (repeated && when.elapsed() <= CONFIRM_TTL).then_some(action)
        });
        let ServeViewState::Active {
            mode,
            transport,
            urls,
            url_index,
            passphrase,
            ..
        } = &mut self.state
        else {
            return ServeAction::Continue;
        };
        let (mode, transport) = (*mode, *transport);
        match code {
            KeyCode::Char('s' | 'S') => match stop_daemon() {
                Ok(()) => self.state = ServeViewState::mode_picker(),
                Err(e) => {
                    self.state = ServeViewState::Error(format!(
                        "Stop failed: {e}. Daemon may still be running; retry or use `aoe serve --stop` from a shell."
                    ))
                }
            },
            KeyCode::Char('g' | 'G') if mode == ServeMode::Tunnel => {
                if confirmed == Some(PendingConfirm::NewPassphrase) {
                    let new_pp = generate_passphrase();
                    save_passphrase_to_disk(&new_pp);
                    self.pending_passphrase = new_pp.clone();
                    self.restart(mode, transport, Some(new_pp));
                } else {
                    self.pending_confirm = Some((PendingConfirm::NewPassphrase, Instant::now()));
                }
            }
            // Tunnel restarts clear all login sessions; Local rebinds the port.
            KeyCode::Char('r' | 'R') => {
                if confirmed == Some(PendingConfirm::Restart) {
                    let pp = (mode == ServeMode::Tunnel).then(|| {
                        passphrase
                            .clone()
                            .unwrap_or_else(|| self.pending_passphrase.clone())
                    });
                    self.restart(mode, transport, pp);
                } else {
                    self.pending_confirm = Some((PendingConfirm::Restart, Instant::now()));
                }
            }
            KeyCode::Tab if urls.len() > 1 => *url_index = (*url_index + 1) % urls.len(),
            KeyCode::Char('?') => self.show_help = true,
            KeyCode::Esc | KeyCode::Char('q') => return ServeAction::Close,
            _ => {}
        }
        ServeAction::Continue
    }

    fn error_key(&mut self, code: KeyCode) -> ServeAction {
        let ServeViewState::Error(msg) = &self.state else {
            return ServeAction::Continue;
        };
        match code {
            // Best effort: no daemon left running is the goal either way.
            KeyCode::Char('s' | 'S') => {
                let _ = stop_daemon();
                ServeAction::Close
            }
            // Clears a stale funnel config blocking port 443; safe when none exists.
            KeyCode::Char('r' | 'R') if error_mentions_tailscale(msg) => {
                self.state = ServeViewState::Error(match run_tailscale_funnel_reset() {
                    Ok(()) => "Ran `tailscale funnel reset`. The existing funnel \
                         config (if any) has been cleared.\n\n\
                         Close this dialog and press R to retry."
                        .to_string(),
                    Err(e) => format!(
                        "`tailscale funnel reset` failed: {e}\n\n\
                         Try running it manually from a shell, then retry."
                    ),
                });
                ServeAction::Continue
            }
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char(' ' | 'q') => ServeAction::Close,
            _ => ServeAction::Continue,
        }
    }

    fn restart(
        &mut self,
        mode: ServeMode,
        transport: Option<TunnelTransport>,
        passphrase: Option<String>,
    ) {
        let log_offset = log_file_size();
        self.state = match restart_daemon(mode, passphrase.as_deref(), transport) {
            Ok(()) => {
                if let Some(pp) = &passphrase {
                    remember_passphrase(pp);
                }
                // A reused Tailscale tunnel is back almost at once; skip the Starting flash.
                std::thread::sleep(Duration::from_millis(200));
                ServeViewState::running(mode, transport, passphrase, log_offset)
            }
            Err(e) => ServeViewState::Error(format!("Restart failed: {}", e)),
        };
    }

    /// Poll the daemon's files and advance the state. Returns true when a redraw is needed.
    pub fn tick(&mut self) -> bool {
        match &mut self.state {
            ServeViewState::ModePicker { flash, .. } => expire_flash(flash),
            ServeViewState::Starting {
                mode,
                transport,
                passphrase,
                started_at,
                log_offset,
            } => {
                let log_grew = log_grew(log_offset);
                let mode = *mode;
                let urls = read_serve_urls();
                if !urls.is_empty() {
                    // A reattached daemon writes serve.passphrase only once startup finishes.
                    let passphrase = passphrase.take().or_else(|| {
                        (mode == ServeMode::Tunnel)
                            .then(recall_passphrase)
                            .flatten()
                    });
                    self.state = ServeViewState::Active {
                        mode,
                        transport: *transport,
                        urls,
                        url_index: 0,
                        passphrase,
                        opened_at: Instant::now(),
                        log_offset: log_file_size(),
                    };
                    return true;
                }
                if crate::cli::serve::daemon_pid().is_none() {
                    let tail = initial_log_tail();
                    let prefix = match mode {
                        ServeMode::Tunnel => {
                            "`aoe serve --remote --daemon` exited before the tunnel came up."
                        }
                        ServeMode::Local => {
                            "`aoe serve --daemon` exited before the server started."
                        }
                    };
                    let hint = diagnose_daemon_exit(&tail.join("\n"), mode);
                    self.state =
                        ServeViewState::Error(format!("{prefix}{hint}{}", last_log_lines(&tail)));
                    return true;
                }
                if mode == ServeMode::Tunnel
                    && started_at.elapsed() > Duration::from_secs(TUNNEL_STARTUP_TIMEOUT_SECS)
                {
                    // Stop it so a tunnel that never came up doesn't linger in the status bar.
                    let stop_note = match stop_daemon() {
                        Ok(()) => "Stuck daemon stopped.".to_string(),
                        Err(e) => format!(
                            "Daemon may still be running \
                             (tried to stop: {}). Stop manually with `aoe serve --stop`.",
                            e
                        ),
                    };
                    self.state = ServeViewState::Error(format!(
                        "HTTPS tunnel did not announce a URL within {}s. \
                         {}\n\n\
                         Most likely cause: Tailscale Funnel needs HTTPS certs \
                         or ACL approval, OR cloudflared is rate-limited / \
                         offline. Re-run with AGENT_OF_EMPIRES_DEBUG=1 and \
                         check debug.log for details.{}",
                        TUNNEL_STARTUP_TIMEOUT_SECS,
                        stop_note,
                        last_log_lines(&initial_log_tail())
                    ));
                    return true;
                }
                log_grew
            }
            ServeViewState::Active { log_offset, .. } => {
                let log_grew = log_grew(log_offset);
                if self
                    .pending_confirm
                    .is_some_and(|(_, t)| t.elapsed() > CONFIRM_TTL)
                {
                    self.pending_confirm = None;
                    return true;
                }
                log_grew
            }
            ServeViewState::Confirm { .. } | ServeViewState::Error(_) => false,
        }
    }

    pub fn render(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        render::render(self, frame, area, theme);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serve_mode_file_token_parses_server_output() {
        for mode in [ServeMode::Local, ServeMode::Tunnel] {
            let written = format!("{}\n", mode.file_token());
            assert_eq!(ServeMode::from_file_token(&written), Some(mode));
        }
        assert_eq!(ServeMode::from_file_token("garbage"), None);
        assert_eq!(ServeMode::from_file_token(""), None);
    }
}
