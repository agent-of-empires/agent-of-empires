//! Serve dialog: chooses how far the local daemon is exposed. The daemon
//! always runs on this machine with at least a localhost listener; this view
//! switches between Localhost, Local network (0.0.0.0) and an HTTPS tunnel by
//! replacing it under the lifecycle transaction, and shows the QR, URL and
//! client command for exposed modes. It never turns the daemon off.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;

use crate::cli::serve::{Exposure, ExposureRequest};
use crate::tui::styles::Theme;

/// Actions returned by [`ServeView::handle_key`], following the
/// full-page takeover pattern used by `SettingsAction` and `DiffAction`.
pub enum ServeAction {
    /// Keep the serve view open; no navigation change.
    Continue,
    /// Close the serve view and return to the home screen.
    Close,
}

/// Which HTTPS tunnel backend the user picked on the Confirm screen.
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

const EXPOSURES: [Exposure; 3] = [Exposure::Localhost, Exposure::Network, Exposure::Tunnel];

fn exposure_label(exposure: Exposure) -> &'static str {
    match exposure {
        Exposure::Localhost => "Localhost only",
        Exposure::Network => "Local network",
        Exposure::Tunnel => "Internet (HTTPS)",
    }
}

/// Transport of a running tunnel, from the mode the daemon recorded.
fn running_transport() -> TunnelTransport {
    let mode = crate::session::get_app_dir()
        .ok()
        .and_then(|dir| std::fs::read_to_string(dir.join("serve.mode")).ok());
    match mode.as_deref().map(str::trim) {
        Some("tunnel") => TunnelTransport::Cloudflare,
        _ => TunnelTransport::Tailscale,
    }
}

pub use crate::cli::serve::{read_serve_urls, ServeUrl};

mod active;
mod passphrase;
mod picker;
mod words;

use active::{render_active, render_error, render_help_overlay, render_qr, ActiveModel};
use passphrase::{
    generate_passphrase, load_or_generate_passphrase, recall_passphrase, remember_passphrase,
    save_passphrase_to_disk,
};
use picker::{render_applying, render_confirm, render_picker, PickerModel};

/// How long an exposed daemon may take to publish its URL after it is ready.
const URL_PUBLISH_TIMEOUT: Duration = Duration::from_secs(10);
/// Log lines kept for a failure report.
const LOG_TAIL_LINES: usize = 200;
/// How long a transient flash stays up.
const FLASH_TTL: Duration = Duration::from_millis(1500);

pub enum ServeViewState {
    /// Where the running daemon is reachable, and the exposure to switch to.
    Picker {
        selected: Exposure,
        /// `None` while no daemon answers (the TUI is reconnecting).
        current: Option<Exposure>,
        /// Localhost URL, when the daemon published one.
        local_url: Option<String>,
        /// Either tailscale OR cloudflared is available.
        tunnel_available: bool,
        /// First non-loopback interface, when there is one.
        network_address: Option<String>,
        flash: Option<(String, Instant)>,
    },
    /// Tunnel-only: risk explanation and transport picker on one screen.
    Confirm {
        selected: TunnelTransport,
        tailscale: TransportStatus,
        cloudflare: TransportStatus,
        flash: Option<(String, Instant)>,
    },
    /// The daemon is being replaced with the `target` exposure.
    Applying {
        target: Exposure,
        transport: Option<TunnelTransport>,
        passphrase: Option<String>,
        started_at: Instant,
        /// `None` once the replacement reported success.
        result: Option<tokio::sync::oneshot::Receiver<Result<(), String>>>,
        ready_at: Option<Instant>,
    },
    /// The daemon is exposed beyond localhost.
    Active {
        mode: Exposure,
        transport: Option<TunnelTransport>,
        urls: Vec<ServeUrl>,
        /// Which `urls` entry is the primary QR target; Tab cycles.
        url_index: usize,
        /// Unknown for a tunnel started outside this TUI without a saved passphrase.
        passphrase: Option<String>,
        opened_at: Instant,
    },
    Error(String),
}

/// A destructive action awaiting confirmation (press the key again).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingConfirm {
    /// Awaiting a second `[G]` press to generate a new passphrase and restart.
    NewPassphrase,
    /// Awaiting a second `[R]` press to restart, clearing tunnel sessions.
    Restart,
}

pub struct ServeView {
    state: ServeViewState,
    /// Passphrase used when the user picks Tunnel. Loaded from
    /// `serve.saved_passphrase` if available, otherwise generated and saved.
    pending_passphrase: String,
    /// Destructive action awaiting a second keypress to confirm.
    pending_confirm: Option<(PendingConfirm, Instant)>,
    show_help: bool,
    /// Pairing code and devices, while exposed.
    pairing: Option<super::pairing::PairingPanel>,
    /// Narrow terminals show the browser/phone section instead of pairing.
    show_web: bool,
    flash: Option<(String, Instant)>,
}

impl Default for ServeView {
    fn default() -> Self {
        Self::new()
    }
}

impl ServeView {
    /// Open on the exposed view when the daemon is reachable beyond
    /// localhost, otherwise on the exposure picker.
    pub fn new() -> Self {
        let mut view = Self {
            state: ServeViewState::Error(String::new()),
            pending_passphrase: load_or_generate_passphrase(),
            pending_confirm: None,
            show_help: false,
            pairing: None,
            show_web: false,
            flash: None,
        };
        match crate::cli::serve::current_exposure() {
            Some(mode @ (Exposure::Network | Exposure::Tunnel)) => view.show_active(mode),
            current => view.show_picker(current, None),
        }
        view
    }

    fn show_picker(&mut self, current: Option<Exposure>, flash: Option<&str>) {
        let tunnel_available = crate::server::tunnel::tailscale_available_sync()
            || crate::server::tunnel::check_cloudflared().is_ok();
        let network_address =
            crate::server::discover_tagged_ips()
                .into_iter()
                .next()
                .map(|(kind, ip)| match kind {
                    crate::server::IpKind::Tailscale => format!("{ip} (Tailscale)"),
                    crate::server::IpKind::Lan => format!("{ip} (LAN)"),
                    crate::server::IpKind::Loopback => format!("{ip} (loopback)"),
                });
        let local_url = matches!(current, Some(Exposure::Localhost))
            .then(read_serve_urls)
            .and_then(|urls| urls.into_iter().next())
            .map(|url| url.url);
        self.state = ServeViewState::Picker {
            selected: current.unwrap_or(Exposure::Localhost),
            current,
            local_url,
            tunnel_available,
            network_address,
            flash: flash.map(|message| (message.to_string(), Instant::now())),
        };
        self.pending_confirm = None;
        self.show_help = false;
        self.pairing = None;
    }

    fn show_active(&mut self, mode: Exposure) {
        let transport = matches!(mode, Exposure::Tunnel).then(running_transport);
        let passphrase = matches!(mode, Exposure::Tunnel)
            .then(recall_passphrase)
            .flatten();
        self.state = ServeViewState::Active {
            mode,
            transport,
            urls: read_serve_urls(),
            url_index: 0,
            passphrase,
            opened_at: Instant::now(),
        };
        self.pending_confirm = None;
        self.show_help = false;
        self.pairing = Some(super::pairing::PairingPanel::open());
        self.show_web = false;
    }

    /// Stop the daemon and start it again on the same exposure, or start one
    /// on localhost when none answers. Rebuilding the binary leaves the
    /// running daemon on the old code, so this is how a new build takes over.
    fn restart(&mut self, mode: Option<Exposure>) {
        let mode = mode.unwrap_or(Exposure::Localhost);
        let tunnel = mode == Exposure::Tunnel;
        let transport = tunnel.then(running_transport);
        let passphrase =
            tunnel.then(|| recall_passphrase().unwrap_or_else(|| self.pending_passphrase.clone()));
        self.apply(mode, transport, passphrase);
    }

    /// Probe tunnel readiness on entering Confirm or pressing `[R]`
    /// after fixing an ACL.
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

    /// A Ready Tailscale beats Cloudflare (stable URL); else whichever is
    /// Ready; else Tailscale so the user sees the fix instructions.
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

    /// Replace the daemon in the background; `tick` follows the result.
    fn apply(
        &mut self,
        target: Exposure,
        transport: Option<TunnelTransport>,
        passphrase: Option<String>,
    ) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            self.state = ServeViewState::Error("No async runtime to change the exposure.".into());
            return;
        };
        let request = match target {
            Exposure::Tunnel => ExposureRequest::Tunnel {
                cloudflare: transport == Some(TunnelTransport::Cloudflare),
                passphrase: passphrase
                    .clone()
                    .unwrap_or_else(|| self.pending_passphrase.clone()),
            },
            Exposure::Network => ExposureRequest::Network,
            Exposure::Localhost => ExposureRequest::Localhost,
        };
        if let Some(passphrase) = &passphrase {
            remember_passphrase(passphrase);
            save_passphrase_to_disk(passphrase);
        }
        let (sender, result) = tokio::sync::oneshot::channel();
        runtime.spawn(async move {
            let outcome = crate::cli::serve::change_exposure(request)
                .await
                .map_err(|error| format!("{error:#}"));
            let _ = sender.send(outcome);
        });
        self.state = ServeViewState::Applying {
            target,
            transport,
            passphrase,
            started_at: Instant::now(),
            result: Some(result),
            ready_at: None,
        };
        self.pending_confirm = None;
        self.show_help = false;
        self.pairing = None;
    }

    /// Act on a picked exposure: apply it, or open the tunnel confirmation.
    fn choose(&mut self, target: Exposure) -> ServeAction {
        let ServeViewState::Picker {
            current,
            tunnel_available,
            network_address,
            flash,
            ..
        } = &mut self.state
        else {
            return ServeAction::Continue;
        };
        let refusal = match target {
            _ if *current == Some(target) && target == Exposure::Localhost => {
                Some("Already reachable from this machine only.")
            }
            Exposure::Network if network_address.is_none() => {
                Some("No non-loopback network interface available.")
            }
            Exposure::Tunnel if !*tunnel_available => {
                Some("Install tailscale or cloudflared to enable Tunnel mode.")
            }
            _ => None,
        };
        if let Some(message) = refusal {
            *flash = Some((message.to_string(), Instant::now()));
            return ServeAction::Continue;
        }
        if *current == Some(target) {
            self.show_active(target);
            return ServeAction::Continue;
        }
        match target {
            Exposure::Tunnel => {
                let (tailscale, cloudflare) = Self::assess_transports();
                self.state = ServeViewState::Confirm {
                    selected: Self::default_transport(tailscale, cloudflare),
                    tailscale,
                    cloudflare,
                    flash: None,
                };
            }
            target => self.apply(target, None, None),
        }
        ServeAction::Continue
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> ServeAction {
        match &mut self.state {
            ServeViewState::Picker {
                selected,
                current,
                flash,
                ..
            } => {
                if flash
                    .as_ref()
                    .is_some_and(|(_, at)| at.elapsed() > FLASH_TTL)
                {
                    *flash = None;
                }
                let index = EXPOSURES.iter().position(|e| e == selected).unwrap_or(0);
                match key.code {
                    KeyCode::Up | KeyCode::Char('k') => {
                        *selected = EXPOSURES[index.saturating_sub(1)];
                        ServeAction::Continue
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        *selected = EXPOSURES[(index + 1).min(EXPOSURES.len() - 1)];
                        ServeAction::Continue
                    }
                    KeyCode::Tab => {
                        *selected = EXPOSURES[(index + 1) % EXPOSURES.len()];
                        ServeAction::Continue
                    }
                    KeyCode::Char(digit @ '1'..='3') => {
                        let target = EXPOSURES[digit as usize - '1' as usize];
                        *selected = target;
                        self.choose(target)
                    }
                    KeyCode::Enter => {
                        let target = *selected;
                        self.choose(target)
                    }
                    KeyCode::Char('r') | KeyCode::Char('R') => {
                        let current = *current;
                        if self
                            .pending_confirm
                            .take()
                            .filter(|(action, at)| {
                                *action == PendingConfirm::Restart
                                    && at.elapsed() <= Duration::from_secs(3)
                            })
                            .is_some()
                        {
                            self.restart(current);
                        } else {
                            self.pending_confirm = Some((PendingConfirm::Restart, Instant::now()));
                            *flash = Some((
                                "Press r again to restart the daemon.".to_string(),
                                Instant::now(),
                            ));
                        }
                        ServeAction::Continue
                    }
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
                if flash
                    .as_ref()
                    .is_some_and(|(_, at)| at.elapsed() > FLASH_TTL)
                {
                    *flash = None;
                }
                let commit = |dialog: &mut ServeView, pick: TunnelTransport| -> ServeAction {
                    let ServeViewState::Confirm {
                        tailscale,
                        cloudflare,
                        flash,
                        ..
                    } = &mut dialog.state
                    else {
                        return ServeAction::Continue;
                    };
                    let status = match pick {
                        TunnelTransport::Tailscale => *tailscale,
                        TunnelTransport::Cloudflare => *cloudflare,
                    };
                    if !status.is_ready() {
                        let message = match (pick, status) {
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
                        *flash = Some((message.to_string(), Instant::now()));
                        return ServeAction::Continue;
                    }
                    let passphrase = dialog.pending_passphrase.clone();
                    dialog.apply(Exposure::Tunnel, Some(pick), Some(passphrase));
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
                        commit(self, TunnelTransport::Tailscale)
                    }
                    KeyCode::Char('c') | KeyCode::Char('C') => {
                        commit(self, TunnelTransport::Cloudflare)
                    }
                    KeyCode::Enter => {
                        let pick = *selected;
                        commit(self, pick)
                    }
                    KeyCode::Char('r') | KeyCode::Char('R') => {
                        let (new_tailscale, new_cloudflare) = ServeView::assess_transports();
                        *tailscale = new_tailscale;
                        *cloudflare = new_cloudflare;
                        *flash = Some(("Refreshed.".to_string(), Instant::now()));
                        ServeAction::Continue
                    }
                    KeyCode::Esc | KeyCode::Char('q') => {
                        self.show_picker(crate::cli::serve::current_exposure(), None);
                        ServeAction::Continue
                    }
                    _ => ServeAction::Continue,
                }
            }
            // The change keeps going in the background after the view closes.
            ServeViewState::Applying { .. } => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => ServeAction::Close,
                _ => ServeAction::Continue,
            },
            ServeViewState::Active {
                mode,
                transport,
                urls,
                url_index,
                ..
            } => {
                if self.show_help {
                    self.show_help = false;
                    return ServeAction::Continue;
                }
                if self
                    .pairing
                    .as_mut()
                    .is_some_and(|panel| panel.handle_key(key))
                {
                    return ServeAction::Continue;
                }
                let confirmed = self
                    .pending_confirm
                    .take()
                    .filter(|(_, at)| at.elapsed() <= Duration::from_secs(3))
                    .map(|(action, _)| action)
                    .filter(|action| match action {
                        PendingConfirm::NewPassphrase => {
                            matches!(key.code, KeyCode::Char('g') | KeyCode::Char('G'))
                        }
                        PendingConfirm::Restart => {
                            matches!(key.code, KeyCode::Char('r') | KeyCode::Char('R'))
                        }
                    });
                let (mode, transport) = (*mode, *transport);
                match key.code {
                    KeyCode::Char('g') | KeyCode::Char('G') if mode == Exposure::Tunnel => {
                        if confirmed == Some(PendingConfirm::NewPassphrase) {
                            let passphrase = generate_passphrase();
                            self.pending_passphrase = passphrase.clone();
                            self.apply(mode, transport, Some(passphrase));
                        } else {
                            self.pending_confirm =
                                Some((PendingConfirm::NewPassphrase, Instant::now()));
                        }
                        ServeAction::Continue
                    }
                    KeyCode::Char('r') | KeyCode::Char('R') => {
                        if confirmed == Some(PendingConfirm::Restart) {
                            self.restart(Some(mode));
                        } else {
                            self.pending_confirm = Some((PendingConfirm::Restart, Instant::now()));
                        }
                        ServeAction::Continue
                    }
                    KeyCode::Char('e') | KeyCode::Char('E') => {
                        self.show_picker(Some(mode), None);
                        ServeAction::Continue
                    }
                    KeyCode::Char('w') | KeyCode::Char('W') => {
                        self.show_web = !self.show_web;
                        ServeAction::Continue
                    }
                    KeyCode::Tab if urls.len() > 1 => {
                        *url_index = (*url_index + 1) % urls.len();
                        ServeAction::Continue
                    }
                    KeyCode::Char('?') => {
                        self.show_help = true;
                        ServeAction::Continue
                    }
                    KeyCode::Esc | KeyCode::Char('q') => ServeAction::Close,
                    _ => ServeAction::Continue,
                }
            }
            ServeViewState::Error(msg) => match key.code {
                KeyCode::Char('r') | KeyCode::Char('R') if error_mentions_tailscale(msg) => {
                    // A stale funnel config commonly blocks port 443; resetting
                    // is safe even when no funnel is configured.
                    self.state = match run_tailscale_funnel_reset() {
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

    /// Whether this view replaces the home screen; the exposed daemon's card
    /// is drawn over it instead.
    pub fn covers_screen(&self) -> bool {
        !matches!(self.state, ServeViewState::Active { .. })
    }

    /// Drive flashes, confirmations and the background exposure change.
    /// Returns true when a redraw is needed.
    pub fn tick(&mut self) -> bool {
        match &mut self.state {
            ServeViewState::Picker { flash, .. } | ServeViewState::Confirm { flash, .. } => {
                if flash
                    .as_ref()
                    .is_some_and(|(_, at)| at.elapsed() > FLASH_TTL)
                {
                    *flash = None;
                    return true;
                }
                false
            }
            ServeViewState::Applying {
                target,
                result,
                ready_at,
                ..
            } => {
                let target = *target;
                if let Some(receiver) = result {
                    match receiver.try_recv() {
                        Err(tokio::sync::oneshot::error::TryRecvError::Empty) => return false,
                        Ok(Ok(())) => {
                            *result = None;
                            *ready_at = Some(Instant::now());
                        }
                        Ok(Err(error)) => {
                            self.state = ServeViewState::Error(apply_error(target, &error));
                            return true;
                        }
                        Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                            self.state = ServeViewState::Error(
                                "The exposure change was interrupted.".to_string(),
                            );
                            return true;
                        }
                    }
                }
                if target == Exposure::Localhost {
                    self.show_picker(Some(Exposure::Localhost), Some("Now localhost only."));
                    return true;
                }
                if !read_serve_urls().is_empty() {
                    let ServeViewState::Applying {
                        transport,
                        passphrase,
                        ..
                    } = &mut self.state
                    else {
                        return false;
                    };
                    self.state = ServeViewState::Active {
                        mode: target,
                        transport: *transport,
                        urls: read_serve_urls(),
                        url_index: 0,
                        passphrase: passphrase.take(),
                        opened_at: Instant::now(),
                    };
                    self.pairing = Some(super::pairing::PairingPanel::open());
                    self.show_web = false;
                    return true;
                }
                if ready_at.is_some_and(|at| at.elapsed() > URL_PUBLISH_TIMEOUT) {
                    self.state = ServeViewState::Error(
                        "The daemon restarted but published no URL. Check `aoe serve --status`."
                            .to_string(),
                    );
                    return true;
                }
                false
            }
            ServeViewState::Active { .. } => {
                let panel_changed = self.pairing.as_mut().is_some_and(|panel| panel.tick());
                let flash_expired = self
                    .flash
                    .as_ref()
                    .is_some_and(|(_, at)| at.elapsed() > FLASH_TTL);
                if flash_expired {
                    self.flash = None;
                }
                let expired = self
                    .pending_confirm
                    .as_ref()
                    .is_some_and(|(_, at)| at.elapsed() > Duration::from_secs(3));
                if expired {
                    self.pending_confirm = None;
                }
                expired || panel_changed || flash_expired
            }
            ServeViewState::Error(_) => false,
        }
    }

    pub fn render(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        match &self.state {
            ServeViewState::Picker {
                selected,
                current,
                local_url,
                tunnel_available,
                network_address,
                flash,
            } => render_picker(
                frame,
                area,
                theme,
                PickerModel {
                    selected: *selected,
                    current: *current,
                    local_url: local_url.as_deref(),
                    tunnel_available: *tunnel_available,
                    network_address: network_address.as_deref(),
                    flash: flash.as_ref().map(|(m, _)| m.as_str()),
                },
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
            ServeViewState::Applying {
                target, started_at, ..
            } => render_applying(frame, area, theme, *target, started_at.elapsed()),
            ServeViewState::Active {
                mode,
                urls,
                url_index,
                passphrase,
                opened_at,
                ..
            } => {
                let qr = urls
                    .get(*url_index)
                    .or_else(|| urls.first())
                    .map(|url| render_qr(&url.url))
                    .unwrap_or_default();
                render_active(
                    frame,
                    area,
                    theme,
                    &ActiveModel {
                        mode: *mode,
                        urls,
                        url_index: *url_index,
                        passphrase: passphrase.as_deref(),
                        elapsed: opened_at.elapsed(),
                        pending_confirm: self.pending_confirm.as_ref().map(|(a, _)| *a),
                        pairing: self.pairing.as_ref(),
                        show_web: self.show_web,
                        flash: self.flash.as_ref().map(|(text, _)| text.as_str()),
                        qr: &qr,
                    },
                );
                if self.show_help {
                    render_help_overlay(frame, area, theme, *mode);
                }
            }
            ServeViewState::Error(msg) => render_error(frame, area, theme, msg),
        }
    }
}

/// The local daemon the TUI bootstraps, started when it is missing. Used by
/// explicit user actions that need the daemon API right away.
pub(crate) async fn start_local_daemon_and_wait(
) -> Result<crate::acp::client::DaemonEndpoint, String> {
    crate::acp::client::daemon_manager::ensure_local_daemon("")
        .await
        .map_err(|error| format!("{error:#}"))
}

fn apply_error(target: Exposure, error: &str) -> String {
    let tail = initial_log_tail();
    let hint = diagnose_daemon_exit(&tail.join("\n"), target);
    let compact: Vec<String> = tail.iter().map(|l| compact_log_line(l)).collect();
    let detail = if compact.is_empty() {
        String::new()
    } else {
        format!("\n\nLast log lines:\n{}", compact.join("\n"))
    };
    format!(
        "Could not switch to {}: {error}{hint}{detail}",
        exposure_label(target)
    )
}

/// Map a common errno string in the daemon log tail to a one-line hint,
/// prefixed with a blank line, or `""` when nothing is recognized.
fn diagnose_daemon_exit(log: &str, target: Exposure) -> &'static str {
    if log.contains("EADDRNOTAVAIL") || log.contains("Cannot assign requested address") {
        return match target {
            Exposure::Network => {
                "\n\nHint: the interface we tried to bind on went away. \
                 Is Tailscale still up?"
            }
            Exposure::Localhost | Exposure::Tunnel => "",
        };
    }
    if log.contains("EADDRINUSE") || log.contains("Address already in use") {
        return "\n\nHint: another process holds the daemon's port. \
                Free it, or delete serve.last_port in the app directory to pick a new one.";
    }
    if log.contains("Permission denied") {
        return "\n\nHint: permission denied on bind. Are you trying a \
                privileged port (<1024)? We normally pick a high port.";
    }
    ""
}

/// The `aoe remote add` line another machine runs, or `None` for a loopback
/// URL no other machine can use. It prompts for the pairing code.
fn client_command(url: &str) -> Option<String> {
    let base = base_url(url)?;
    let host = base
        .host_str()?
        .trim_start_matches('[')
        .trim_end_matches(']');
    let loopback = host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    (!loopback).then(|| {
        format!(
            "aoe remote add {}",
            crate::daemon::remotes::short_remote_address(base.as_str())
        )
    })
}

/// `url` without its path and query, where a credential may ride.
fn base_url(url: &str) -> Option<reqwest::Url> {
    let mut base = reqwest::Url::parse(url).ok()?;
    base.set_query(None);
    base.set_path("/");
    Some(base)
}

fn log_file_path() -> Option<PathBuf> {
    crate::cli::serve::stdio_redirect_path().ok()
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
fn truncate_to_width(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    let kept: String = text.chars().take(width.saturating_sub(1)).collect();
    format!("{kept}\u{2026}")
}

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

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
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

/// Whether to offer the `[R]` reset keybind for a Tailscale/funnel error.
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

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn diagnose_daemon_exit_recognizes_common_errnos() {
        let unavailable = "ERROR: bind: Cannot assign requested address";
        for (log, target, needle) in [
            (unavailable, Exposure::Network, Some("interface")),
            (unavailable, Exposure::Tunnel, None),
            ("Address already in use", Exposure::Network, Some("port")),
            ("Permission denied", Exposure::Tunnel, Some("permission")),
            ("some unrelated line", Exposure::Network, None),
        ] {
            let hint = diagnose_daemon_exit(log, target);
            match needle {
                Some(needle) => assert!(hint.contains(needle), "{log:?}: {hint:?}"),
                None => assert_eq!(hint, "", "{log:?}"),
            }
        }
    }

    fn picker(current: Option<Exposure>) -> ServeView {
        ServeView {
            state: ServeViewState::Picker {
                selected: current.unwrap_or(Exposure::Localhost),
                current,
                local_url: None,
                tunnel_available: false,
                network_address: None,
                flash: None,
            },
            pending_passphrase: "pass".into(),
            pending_confirm: None,
            show_help: false,
            pairing: None,
            show_web: false,
            flash: None,
        }
    }

    fn press(view: &mut ServeView, code: KeyCode) {
        view.handle_key(KeyEvent::new(code, crossterm::event::KeyModifiers::NONE));
    }

    /// Unavailable or unchanged exposures explain themselves instead of
    /// restarting the daemon.
    #[test]
    fn picker_refuses_unusable_choices_without_restarting() {
        for (current, keys, flash) in [
            (
                Some(Exposure::Localhost),
                vec![KeyCode::Enter],
                "Already reachable from this machine only.",
            ),
            (
                Some(Exposure::Localhost),
                vec![KeyCode::Down, KeyCode::Enter],
                "No non-loopback network interface available.",
            ),
            (
                None,
                vec![KeyCode::Char('3')],
                "Install tailscale or cloudflared to enable Tunnel mode.",
            ),
        ] {
            let mut view = picker(current);
            for key in keys {
                press(&mut view, key);
            }
            let ServeViewState::Picker {
                flash: Some((message, _)),
                ..
            } = &view.state
            else {
                panic!("expected a flash on the picker for {current:?}");
            };
            assert_eq!(message, flash);
        }
    }

    /// A rebuilt binary only takes over once the daemon restarts, so the
    /// picker offers one. It costs every session its connection, so the first
    /// press only asks.
    #[test]
    fn picker_restart_asks_before_it_replaces_the_daemon() {
        let mut view = picker(Some(Exposure::Localhost));
        press(&mut view, KeyCode::Char('r'));
        assert!(matches!(view.state, ServeViewState::Picker { .. }));
        assert_eq!(
            view.pending_confirm.map(|(action, _)| action),
            Some(PendingConfirm::Restart)
        );

        press(&mut view, KeyCode::Char('r'));
        // No async runtime in a unit test, so the restart reports that rather
        // than reaching the daemon; either way the picker is left behind.
        assert!(!matches!(view.state, ServeViewState::Picker { .. }));
    }

    #[test]
    fn picker_navigation_stays_in_bounds() {
        let mut view = picker(Some(Exposure::Localhost));
        let selected = |view: &ServeView| match &view.state {
            ServeViewState::Picker { selected, .. } => *selected,
            _ => panic!("left the picker"),
        };
        press(&mut view, KeyCode::Up);
        assert_eq!(selected(&view), Exposure::Localhost);
        for _ in 0..4 {
            press(&mut view, KeyCode::Down);
        }
        assert_eq!(selected(&view), Exposure::Tunnel);
        press(&mut view, KeyCode::Tab);
        assert_eq!(selected(&view), Exposure::Localhost);
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
}
