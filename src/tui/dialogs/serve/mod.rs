//! Remote access view: a controller for the `aoe serve --daemon` lifecycle, which outlives the TUI.

mod daemon;
mod render;
mod words;

use std::cell::RefCell;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;

use crate::acp::client::autostart::initial_log_tail;
use crate::cli::serve::{read_serve_urls, ServeLaunch, ServeUrl};
use crate::tui::components::hover::HoverState;
use crate::tui::dialogs::{contains, hit, target_rects};
use crate::tui::styles::Theme;
use crate::tui::worker::Worker;
use daemon::*;

const TUNNEL_STARTUP_TIMEOUT_SECS: u64 = 60;
const FLASH_TTL: Duration = Duration::from_millis(1500);
const CONFIRM_TTL: Duration = Duration::from_secs(3);

pub enum ServeAction {
    Launch(ServeLaunchRequest),
    Stop,
    Continue,
    Close,
}
pub struct ServeLaunchRequest {
    restart: bool,
    mode: ServeMode,
    transport: Option<TunnelTransport>,
    passphrase: Option<String>,
}

impl ServeLaunchRequest {
    pub(crate) async fn run(
        self,
        profile: String,
        predecessor_epoch: Option<String>,
    ) -> Result<
        (
            crate::daemon::RuntimeConnection,
            crate::daemon::DaemonClient,
        ),
        String,
    > {
        use clap::Parser;
        let port = crate::acp::client::autostart::load_or_generate_port().to_string();
        let mut argv = vec!["aoe", "serve", "--daemon", "--port", port.as_str()];
        match self.mode {
            ServeMode::Local => argv.extend(["--host", "0.0.0.0"]),
            ServeMode::Tunnel => {
                argv.extend(["--remote", "--host", "127.0.0.1"]);
                if self.transport == Some(TunnelTransport::Cloudflare) {
                    argv.push("--no-tailscale");
                }
            }
        }
        let parsed = crate::cli::Cli::try_parse_from(argv).map_err(|error| error.to_string())?;
        let Some(crate::cli::Commands::Serve(mut args)) = parsed.command else {
            unreachable!();
        };
        args.passphrase = self.passphrase.clone();
        let result = crate::cli::serve::start_dashboard_for_tui(&profile, args, predecessor_epoch.as_deref(), self.restart).await
            .map_err(|error| {
                if error.downcast_ref::<crate::session::LoadSource>().is_some()
                    || matches!(error.downcast_ref::<crate::daemon::RuntimeConnectionError>(), Some(crate::daemon::RuntimeConnectionError::Protocol))
                {
                    crate::session::sanitized_load_failure(&error)
                } else {
                    tracing::warn!(target: "tui.home", %error, "dashboard launch failed");
                    "Dashboard start failed. Check `aoe logs`; use `aoe serve --status` to inspect the current daemon. If replacement stopped its predecessor, restore the retained launch explicitly with `aoe serve --rollback`.".to_owned()
                }
            });
        if result.is_err()
            && initial_log_tail()
                .iter()
                .any(|line| line.contains("EADDRINUSE") || line.contains("Address already in use"))
        {
            remove_app_file("serve.last_port");
        }
        if result.is_ok() {
            remember_last_mode(self.mode);
        }
        // A failed reconnect can follow a successor that already runs the new passphrase.
        if let Some(passphrase) = self.passphrase {
            if result.is_ok() || crate::cli::serve::daemon_runs_passphrase(&passphrase) {
                remember_passphrase(&passphrase);
                save_passphrase_to_disk(&passphrase);
            }
        }
        result
    }
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

/// What a probe checks; each shells out to `ps` on macOS, so it runs on a worker.
enum Probe {
    /// A web daemon is running.
    WebDaemon,
    /// Any daemon is running.
    Daemon,
}

fn run_probe(probe: Probe) -> bool {
    match probe {
        Probe::WebDaemon => crate::cli::serve::web_daemon_pid().is_some(),
        Probe::Daemon => crate::cli::serve::daemon_pid().is_some(),
    }
}

/// Probes run one at a time on a worker. Each carries an attempt id, and a result applies
/// only while the state it was requested for is still waiting on that attempt.
struct Probes {
    worker: Worker<(u64, Probe), (u64, bool)>,
    last_attempt: u64,
    /// The in-flight liveness probe for `Starting`.
    liveness: Option<u64>,
}

impl Probes {
    fn new(mut handler: impl FnMut(Probe) -> bool + Send + 'static) -> Self {
        Self {
            worker: Worker::spawn("aoe-serve-dialog-probe", move |(attempt, probe)| {
                (attempt, handler(probe))
            }),
            last_attempt: 0,
            liveness: None,
        }
    }

    fn request(&mut self, probe: Probe) -> u64 {
        self.last_attempt += 1;
        self.worker.request((self.last_attempt, probe));
        self.last_attempt
    }
}

enum ServeViewState {
    /// Waiting on the probe that decides between reattaching and the mode picker.
    Checking { attempt: u64 },
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
    /// What failed, kept so a Tunnel failure can offer the other provider.
    Error {
        message: String,
        mode: ServeMode,
        transport: Option<TunnelTransport>,
    },
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

    /// The provider screen after a Tunnel failure: the provider that did not just fail is
    /// preselected when it is ready.
    fn provider_retry(
        failed: Option<TunnelTransport>,
        tailscale: TransportStatus,
        cloudflare: TransportStatus,
    ) -> Self {
        let other = match failed {
            Some(TunnelTransport::Tailscale) => Some((TunnelTransport::Cloudflare, cloudflare)),
            Some(TunnelTransport::Cloudflare) => Some((TunnelTransport::Tailscale, tailscale)),
            None => None,
        };
        let selected = match other {
            Some((transport, TransportStatus::Ready)) => transport,
            _ => default_transport(tailscale, cloudflare),
        };
        ServeViewState::Confirm {
            selected,
            tailscale,
            cloudflare,
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

/// What the last frame drew under the mouse, recorded through a `RefCell`
/// since rendering borrows the view immutably.
#[derive(Default)]
struct ServeMouse {
    /// Hints and cards, each standing for a key.
    keys: Vec<(KeyEvent, Rect)>,
    /// Values a click copies, keyed by `(label, value)`.
    copies: Vec<((&'static str, String), Rect)>,
    hover: HoverState,
}

impl ServeMouse {
    fn rects(&self) -> Vec<Rect> {
        let mut rects = target_rects(&self.keys);
        rects.extend(target_rects(&self.copies));
        rects
    }
}

/// What a restart does to the link a Device opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinkChange {
    Same,
    /// The link changes, but the Device's cookies still let it in: another port (cookies are
    /// host-only, not per port), or a new token behind a passphrase sign-in.
    Moved,
    /// The Device's cookies no longer let it in: another host, which they are not sent to, or
    /// a new token that the `aoe_token` cookie of a token-only Device doesn't match.
    Replaced,
}

/// What a restart does to passphrase sign-ins on an unchanged host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Logins {
    /// No passphrase wall, or the sign-ins persist across the restart.
    Kept,
    /// The next start doesn't persist them, or the running daemon only held them in memory.
    Lost,
    /// A reattached daemon's login persistence is unknown.
    Unknown,
}

/// What a restart does to Devices. Agents keep running either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RestartEffect {
    link: LinkChange,
    logins: Logins,
    /// The next daemon puts Devices behind the passphrase login wall.
    passphrase: bool,
    /// The token is near expiry, so a standing help screen cannot say the link stays.
    may_change: bool,
}

/// What the next start will do, read when R, G or help states it: the restarted daemon reads
/// all of it afresh.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct RestartProbe {
    /// The restart takes the server's Funnel path (Tunnel only).
    on_funnel: bool,
    /// `auth.persist_sessions` as the next start reads it.
    persists: bool,
    /// The port a Local restart reuses; `None` means a fresh random one.
    port: Option<u16>,
    /// The hosts a Local restart publishes: this machine's addresses plus `localhost`.
    local_hosts: Vec<String>,
    /// The recorded named tunnel's host, which a Tunnel restart replays (Tunnel only).
    named_host: Option<String>,
    /// A Local restart inherits `AOE_SERVE_PASSPHRASE` from this process.
    local_passphrase: bool,
    /// Whether a Local restart mints a new access token.
    local_token: TokenWindow,
}

impl RestartProbe {
    fn now(mode: ServeMode, transport: Option<TunnelTransport>) -> Self {
        let recorded = crate::cli::serve::recorded_launch();
        match mode {
            ServeMode::Local => RestartProbe {
                on_funnel: false,
                persists: persist_sessions(),
                port: saved_port(),
                local_hosts: crate::server::discover_tagged_ips()
                    .into_iter()
                    .map(|(_, ip)| ip.to_string())
                    .chain(["localhost".to_string()])
                    .collect(),
                local_passphrase: std::env::var("AOE_SERVE_PASSPHRASE")
                    .is_ok_and(|pp| !pp.is_empty()),
                local_token: effective_token_window(recorded.as_ref(), token_window()),
                named_host: None,
            },
            ServeMode::Tunnel => RestartProbe {
                on_funnel: restart_on_funnel(transport),
                persists: persist_sessions(),
                named_host: recorded
                    .and_then(|launch| launch.tunnel_name.and(launch.tunnel_url))
                    .and_then(|url| link_host(&url).map(str::to_owned)),
                ..RestartProbe::default()
            },
        }
    }
}

impl RestartEffect {
    /// A Tunnel keeps its host when the recorded named tunnel is replayed, or when it is on
    /// Funnel and the restart takes the Funnel path again; otherwise the server opens a fresh
    /// quick tunnel. A Local daemon republishes this machine's addresses on
    /// the reused or a random port, with a new token once the old one has aged out. Behind the
    /// passphrase wall, sign-ins then survive only if the running daemon wrote them to disk and
    /// the next start reads them back.
    fn of(
        mode: ServeMode,
        urls: &[ServeUrl],
        probe: &RestartProbe,
        running_persists: Option<bool>,
    ) -> Self {
        let (link, passphrase) = match mode {
            ServeMode::Local => {
                let hosts_stay = urls.iter().all(|u| {
                    link_host(&u.url).is_some_and(|host| {
                        is_loopback_host(host) || probe.local_hosts.iter().any(|h| h == host)
                    })
                });
                let port_stays = probe
                    .port
                    .is_some_and(|port| urls.iter().all(|u| link_port(&u.url) == Some(port)));
                let renews = probe.local_token == TokenWindow::Renews;
                let link = if !hosts_stay || (renews && !probe.local_passphrase) {
                    LinkChange::Replaced
                } else if !port_stays || renews {
                    LinkChange::Moved
                } else {
                    LinkChange::Same
                };
                (link, probe.local_passphrase)
            }
            ServeMode::Tunnel => {
                let stays = match &probe.named_host {
                    Some(named) => urls
                        .iter()
                        .any(|u| link_host(&u.url).is_some_and(|host| host == named)),
                    None => probe.on_funnel && urls.iter().any(|u| is_funnel_link(&u.url)),
                };
                (
                    if stays {
                        LinkChange::Same
                    } else {
                        LinkChange::Replaced
                    },
                    true,
                )
            }
        };
        let logins = if !passphrase {
            Logins::Kept
        } else if !probe.persists {
            Logins::Lost
        } else {
            match running_persists {
                Some(true) => Logins::Kept,
                Some(false) => Logins::Lost,
                None => Logins::Unknown,
            }
        };
        RestartEffect {
            link,
            logins,
            passphrase,
            may_change: mode == ServeMode::Local && probe.local_token == TokenWindow::Boundary,
        }
    }
}

/// Only token auth puts a token in the link, and a restart replays the recorded auth mode.
fn effective_token_window(recorded: Option<&ServeLaunch>, window: TokenWindow) -> TokenWindow {
    let token_auth = recorded.is_none_or(|launch| {
        launch.core_only || launch.auth_mode == crate::cli::serve::AuthMode::Token
    });
    if token_auth {
        window
    } else {
        TokenWindow::Reused
    }
}

fn persist_sessions() -> bool {
    crate::session::config::load_config()
        .ok()
        .flatten()
        .is_none_or(|config| config.auth.persist_sessions)
}

/// Mirrors the server's choice: `--no-tailscale` for an explicit Cloudflare pick, otherwise
/// Funnel whenever `tailscale` is installed and logged in.
fn restart_on_funnel(transport: Option<TunnelTransport>) -> bool {
    transport != Some(TunnelTransport::Cloudflare)
        && crate::server::tunnel::tailscale_available_sync()
}

fn link_authority(url: &str) -> Option<&str> {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    rest.split(['/', '?', '#']).next()
}

fn link_host(url: &str) -> Option<&str> {
    let authority = link_authority(url)?;
    Some(
        authority
            .rsplit_once(':')
            .map_or(authority, |(host, _)| host),
    )
}

/// A Local restart binds 0.0.0.0, which keeps these reachable. IPv6 is not covered: it is not
/// bound, and `link_host` does not parse bracketed hosts.
fn is_loopback_host(host: &str) -> bool {
    host == "localhost"
        || host
            .parse::<std::net::Ipv4Addr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// The explicit port in a Local link such as `http://192.168.1.42:51234/?token=…`.
fn link_port(url: &str) -> Option<u16> {
    link_authority(url)?.rsplit_once(':')?.1.parse().ok()
}

/// Funnel serves the machine's stable `https://<host>.<tailnet>.ts.net` origin.
fn is_funnel_link(url: &str) -> bool {
    link_host(url).is_some_and(|host| host.ends_with(".ts.net"))
}

/// The error for a daemon that vanished before publishing its URL.
fn exited_before_ready(mode: ServeMode, transport: Option<TunnelTransport>) -> ServeViewState {
    let tail = initial_log_tail();
    let prefix = match mode {
        ServeMode::Tunnel => "`aoe serve --remote --daemon` exited before the tunnel came up.",
        ServeMode::Local => "`aoe serve --daemon` exited before the server started.",
    };
    let hint = diagnose_daemon_exit(&tail.join("\n"), mode);
    ServeViewState::Error {
        message: format!("{prefix}{hint}{}", last_log_lines(&tail)),
        mode,
        transport,
    }
}

fn stop_failed(e: &str) -> String {
    format!("Stop failed: {e}. Daemon may still be running; retry or use `aoe serve --stop` from a shell.")
}

/// `tailscale funnel reset` only helps a Tunnel that went through Funnel; a reattached
/// daemon's transport is unknown, so its message decides.
fn funnel_reset_applies(
    message: &str,
    mode: ServeMode,
    transport: Option<TunnelTransport>,
) -> bool {
    mode == ServeMode::Tunnel
        && transport.map_or_else(
            || error_mentions_tailscale(message),
            |t| t == TunnelTransport::Tailscale,
        )
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
    mouse: RefCell<ServeMouse>,
    /// The label of the value a click last copied, shown for `FLASH_TTL`.
    copied: Option<(&'static str, Instant)>,
    /// The setting the running daemon started with, when this view started it; `None` for a
    /// reattached daemon.
    running_persists: Option<bool>,
    /// What a restart would do now. Refreshed when R, G or help is pressed rather than per
    /// frame: the probe shells out to `tailscale`.
    restart_probe: RestartProbe,
    pending_launch: Option<ServeLaunchRequest>,
    launch_in_flight: bool,
    stop_requested: bool,
    probes: Probes,
}

impl Default for ServeView {
    fn default() -> Self {
        Self::new()
    }
}

impl ServeView {
    /// Reattaches to a running daemon, otherwise opens the mode picker.
    pub fn new() -> Self {
        Self::with_probe_handler(run_probe)
    }

    /// A view past its construction probe, for tests that need the settled screen.
    #[cfg(test)]
    pub(crate) fn settled() -> Self {
        let mut view = Self::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while matches!(view.state, ServeViewState::Checking { .. }) {
            view.tick();
            assert!(
                Instant::now() < deadline,
                "construction probe never settled"
            );
            std::thread::yield_now();
        }
        view
    }

    fn with_probe_handler(handler: impl FnMut(Probe) -> bool + Send + 'static) -> Self {
        let mut probes = Probes::new(handler);
        let attempt = probes.request(Probe::WebDaemon);
        Self {
            state: ServeViewState::Checking { attempt },
            pending_passphrase: load_or_generate_passphrase(),
            pending_confirm: None,
            show_help: false,
            mouse: RefCell::default(),
            copied: None,
            running_persists: None,
            restart_probe: RestartProbe::default(),
            pending_launch: None,
            launch_in_flight: false,
            stop_requested: false,
            probes,
        }
    }

    /// Hints and cards return their key; the URL, token and passphrase copy
    /// on click. Any click closes help, like any key.
    pub fn handle_click(&mut self, col: u16, row: u16) -> Option<KeyEvent> {
        if self.show_help {
            return Some(KeyEvent::from(KeyCode::Esc));
        }
        let mouse = self.mouse.get_mut();
        let copy = mouse
            .copies
            .iter()
            .find(|(_, rect)| contains(*rect, col, row))
            .map(|(copy, _)| copy.clone());
        if let Some((label, value)) = copy {
            crate::tui::clipboard::copy_to_clipboard(&value);
            self.copied = Some((label, Instant::now()));
            return None;
        }
        hit(&mouse.keys, col, row)
    }

    pub fn handle_hover(&mut self, col: u16, row: u16) -> bool {
        let mouse = self.mouse.get_mut();
        let rects = mouse.rects();
        mouse.hover.update(col, row, &rects)
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> ServeAction {
        if matches!(key.code, KeyCode::Char('s' | 'S'))
            && matches!(
                self.state,
                ServeViewState::Starting { .. }
                    | ServeViewState::Active { .. }
                    | ServeViewState::Error { .. }
            )
        {
            if self.stop_requested {
                return ServeAction::Continue;
            }
            self.begin_stop();
            return ServeAction::Stop;
        }
        if self.stop_requested {
            return if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
                ServeAction::Close
            } else {
                ServeAction::Continue
            };
        }
        let action = match self.state {
            ServeViewState::Checking { .. } => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => ServeAction::Close,
                _ => ServeAction::Continue,
            },
            ServeViewState::ModePicker { .. } => self.mode_picker_key(key.code),
            ServeViewState::Confirm { .. } => self.confirm_key(key.code),
            ServeViewState::Starting { .. } => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => ServeAction::Close,
                _ => ServeAction::Continue,
            },
            ServeViewState::Active { .. } => self.active_key(key.code),
            ServeViewState::Error { .. } => self.error_key(key.code),
        };
        self.pending_launch
            .take()
            .map_or(action, ServeAction::Launch)
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
        let passphrase = (mode == ServeMode::Tunnel).then(|| self.pending_passphrase.clone());
        // Read just before the launch, which the daemon reads at startup.
        self.running_persists = Some(persist_sessions());
        self.pending_launch = Some(ServeLaunchRequest {
            restart: false,
            mode,
            transport,
            passphrase: passphrase.clone(),
        });
        self.launch_in_flight = true;
        self.state = ServeViewState::Starting {
            mode,
            transport,
            passphrase,
            started_at: Instant::now(),
            log_offset: log_file_size(),
        };
    }

    pub(crate) fn finish_launch(&mut self, result: Result<(), String>) {
        self.launch_in_flight = false;
        let ServeViewState::Starting {
            mode,
            transport,
            passphrase,
            log_offset,
            ..
        } = &self.state
        else {
            return;
        };
        self.state = match result {
            Ok(()) => ServeViewState::running(*mode, *transport, passphrase.clone(), *log_offset),
            Err(message) => ServeViewState::Error {
                message,
                mode: *mode,
                transport: *transport,
            },
        };
    }

    pub(crate) fn begin_stop(&mut self) {
        self.stop_requested = true;
        self.launch_in_flight = true;
    }

    pub(crate) fn finish_stop(&mut self, result: Result<(), String>) {
        self.stop_requested = false;
        self.launch_in_flight = false;
        let (mode, transport) = match &self.state {
            ServeViewState::Checking { .. } => (ServeMode::Local, None),
            ServeViewState::ModePicker { selected, .. } => (*selected, None),
            ServeViewState::Confirm { selected, .. } => (ServeMode::Tunnel, Some(*selected)),
            ServeViewState::Starting {
                mode, transport, ..
            }
            | ServeViewState::Active {
                mode, transport, ..
            }
            | ServeViewState::Error {
                mode, transport, ..
            } => (*mode, *transport),
        };
        self.state = match result {
            Ok(()) => ServeViewState::mode_picker(),
            Err(message) => ServeViewState::Error {
                message,
                mode,
                transport,
            },
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
            KeyCode::Char('g' | 'G') if mode == ServeMode::Tunnel => {
                if confirmed == Some(PendingConfirm::NewPassphrase) {
                    let new_pp = generate_passphrase();
                    self.pending_passphrase = new_pp.clone();
                    self.restart(mode, transport, Some(new_pp));
                } else {
                    self.restart_probe = RestartProbe::now(mode, transport);
                    self.pending_confirm = Some((PendingConfirm::NewPassphrase, Instant::now()));
                }
            }
            // Reusing the passphrase keeps persisted logins; see `RestartEffect`.
            KeyCode::Char('r' | 'R') => {
                let fresh = RestartProbe::now(mode, transport);
                let unchanged = confirmed == Some(PendingConfirm::Restart)
                    && RestartEffect::of(mode, urls, &fresh, self.running_persists)
                        == RestartEffect::of(
                            mode,
                            urls,
                            &self.restart_probe,
                            self.running_persists,
                        );
                if unchanged {
                    let pp = (mode == ServeMode::Tunnel).then(|| {
                        passphrase
                            .clone()
                            .unwrap_or_else(|| self.pending_passphrase.clone())
                    });
                    self.restart(mode, transport, pp);
                } else {
                    self.restart_probe = fresh;
                    self.pending_confirm = Some((PendingConfirm::Restart, Instant::now()));
                }
            }
            KeyCode::Tab if urls.len() > 1 => *url_index = (*url_index + 1) % urls.len(),
            KeyCode::Char('?') => {
                self.restart_probe = RestartProbe::now(mode, transport);
                self.show_help = true;
            }
            KeyCode::Esc | KeyCode::Char('q') => return ServeAction::Close,
            _ => {}
        }
        ServeAction::Continue
    }

    fn error_key(&mut self, code: KeyCode) -> ServeAction {
        let ServeViewState::Error {
            message,
            mode,
            transport,
        } = &self.state
        else {
            return ServeAction::Continue;
        };
        let (mode, transport) = (*mode, *transport);
        match code {
            KeyCode::Char('p' | 'P') if mode == ServeMode::Tunnel => {
                // Like S: a daemon left running must not outlive the retry.
                if crate::cli::serve::daemon_pid().is_some() {
                    if let Err(e) = stop_daemon() {
                        self.state = ServeViewState::Error {
                            message: stop_failed(&e),
                            mode,
                            transport,
                        };
                        return ServeAction::Continue;
                    }
                }
                let (tailscale, cloudflare) = assess_transports();
                self.state = ServeViewState::provider_retry(transport, tailscale, cloudflare);
                ServeAction::Continue
            }
            // Clears a stale funnel config blocking port 443; safe when none exists.
            KeyCode::Char('r' | 'R') if funnel_reset_applies(message, mode, transport) => {
                let message = match run_tailscale_funnel_reset() {
                    Ok(()) => "Ran `tailscale funnel reset`. The existing funnel \
                         config (if any) has been cleared.\n\n\
                         Press P to pick a provider and start again."
                        .to_string(),
                    Err(e) => format!(
                        "`tailscale funnel reset` failed: {e}\n\n\
                         Try running it manually from a shell, then press P to start again."
                    ),
                };
                self.state = ServeViewState::Error {
                    message,
                    mode,
                    transport,
                };
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
        self.running_persists = Some(persist_sessions());
        self.pending_launch = Some(ServeLaunchRequest {
            restart: true,
            mode,
            transport,
            passphrase: passphrase.clone(),
        });
        self.launch_in_flight = true;
        self.state = ServeViewState::Starting {
            mode,
            transport,
            passphrase,
            started_at: Instant::now(),
            log_offset: log_file_size(),
        };
    }

    /// Poll the daemon's files and advance the state. Returns true when a redraw is needed.
    pub fn tick(&mut self) -> bool {
        let mut changed = self.apply_probes();
        if self.launch_in_flight {
            self.probes.liveness = None;
            return changed;
        }
        changed |= self.poll_files();
        if !matches!(self.state, ServeViewState::Starting { .. }) {
            self.probes.liveness = None;
        } else if self.probes.liveness.is_none() {
            self.probes.liveness = Some(self.probes.request(Probe::Daemon));
        }
        changed
    }

    fn apply_probes(&mut self) -> bool {
        use std::sync::mpsc::TryRecvError;
        let mut changed = false;
        loop {
            match self.probes.worker.try_recv() {
                Ok((attempt, running)) => changed |= self.apply_probe(attempt, running),
                Err(TryRecvError::Empty) => return changed,
                Err(TryRecvError::Disconnected) => return changed | self.fail_pending(),
            }
        }
    }

    fn apply_probe(&mut self, attempt: u64, running: bool) -> bool {
        match &self.state {
            ServeViewState::Checking { attempt: want } if *want == attempt => {
                self.state = if running {
                    // Daemons that predate `serve.mode` could only be Tunnel.
                    let mode = read_serve_mode().unwrap_or(ServeMode::Tunnel);
                    let passphrase = (mode == ServeMode::Tunnel)
                        .then(recall_passphrase)
                        .flatten();
                    ServeViewState::running(mode, None, passphrase, log_file_size())
                } else {
                    ServeViewState::mode_picker()
                };
                true
            }
            ServeViewState::Starting {
                mode, transport, ..
            } if self.probes.liveness == Some(attempt) => {
                self.probes.liveness = None;
                // A published URL outranks a probe that saw the daemon exit.
                if running || !read_serve_urls().is_empty() {
                    return false;
                }
                self.state = exited_before_ready(*mode, *transport);
                true
            }
            _ => false,
        }
    }

    /// A state waiting on a dead worker becomes an error instead of hanging.
    fn fail_pending(&mut self) -> bool {
        let (mode, transport) = match &self.state {
            ServeViewState::Checking { .. } => (ServeMode::Local, None),
            ServeViewState::Starting {
                mode, transport, ..
            } if self.probes.liveness.is_some() => (*mode, *transport),
            _ => return false,
        };
        self.probes.liveness = None;
        self.state = ServeViewState::Error {
            message: "The background probe worker stopped.".to_string(),
            mode,
            transport,
        };
        true
    }

    fn poll_files(&mut self) -> bool {
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
                    let message = format!(
                        "HTTPS tunnel did not announce a URL within {}s. \
                         {}\n\n\
                         Most likely cause: Tailscale Funnel needs HTTPS certs \
                         or ACL approval, OR cloudflared is rate-limited / \
                         offline. Re-run with AGENT_OF_EMPIRES_DEBUG=1 and \
                         check debug.log for details.{}",
                        TUNNEL_STARTUP_TIMEOUT_SECS,
                        stop_note,
                        last_log_lines(&initial_log_tail())
                    );
                    self.state = ServeViewState::Error {
                        message,
                        mode,
                        transport: *transport,
                    };
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
            ServeViewState::Checking { .. }
            | ServeViewState::Confirm { .. }
            | ServeViewState::Error { .. } => false,
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

    /// The restarted daemon reads `auth.persist_sessions` afresh, so a change made while the
    /// view is open must reach the confirm and help.
    #[test]
    #[serial_test::serial]
    fn r_g_and_help_reread_persist_sessions() {
        let _tmp = crate::session::test_support::isolate_app_dir();
        for key in ['R', 'G', '?'] {
            let mut view = ServeView {
                state: ServeViewState::Active {
                    mode: ServeMode::Tunnel,
                    // Cloudflare skips the `tailscale` probe.
                    transport: Some(TunnelTransport::Cloudflare),
                    urls: vec![ServeUrl {
                        label: None,
                        url: "https://host.tailnet.ts.net/".to_string(),
                    }],
                    url_index: 0,
                    passphrase: Some("correct-horse".to_string()),
                    opened_at: Instant::now(),
                    log_offset: 0,
                },
                pending_passphrase: String::new(),
                pending_confirm: None,
                show_help: false,
                mouse: RefCell::default(),
                copied: None,
                running_persists: Some(true),
                restart_probe: RestartProbe {
                    persists: true,
                    ..RestartProbe::default()
                },
                pending_launch: None,
                launch_in_flight: false,
                stop_requested: false,
                probes: Probes::new(|_| false),
            };
            crate::session::config::update_config(|c| c.auth.persist_sessions = false).unwrap();
            view.handle_key(KeyEvent::from(KeyCode::Char(key)));
            assert!(
                !view.restart_probe.persists,
                "{key} kept the value read when the view opened"
            );
            crate::session::config::update_config(|c| c.auth.persist_sessions = true).unwrap();
            view.pending_confirm = None;
            view.show_help = false;
            view.handle_key(KeyEvent::from(KeyCode::Char(key)));
            assert!(view.restart_probe.persists, "{key}");
        }
    }

    #[test]
    fn a_tunnel_retry_preselects_the_provider_that_did_not_fail() {
        use TransportStatus::{FunnelNotEnabled, NotInstalled, Ready};
        use TunnelTransport::{Cloudflare, Tailscale};
        // (failed provider, tailscale, cloudflare, preselected)
        let cases = [
            (Some(Tailscale), Ready, Ready, Cloudflare),
            // The other provider isn't ready, so the usual default applies.
            (Some(Tailscale), Ready, NotInstalled, Tailscale),
            (Some(Cloudflare), Ready, Ready, Tailscale),
            (Some(Cloudflare), FunnelNotEnabled, Ready, Cloudflare),
            // A reattached daemon's transport is unknown.
            (None, Ready, Ready, Tailscale),
            (None, NotInstalled, Ready, Cloudflare),
        ];
        for (failed, ts, cf, want) in cases {
            let ServeViewState::Confirm {
                selected,
                tailscale,
                cloudflare,
                flash,
            } = ServeViewState::provider_retry(failed, ts, cf)
            else {
                panic!("{failed:?} {ts:?} {cf:?}: not the provider screen");
            };
            assert_eq!(
                (selected, tailscale, cloudflare, flash.is_none()),
                (want, ts, cf, true),
                "{failed:?} {ts:?} {cf:?}"
            );
        }
    }

    #[test]
    fn a_local_error_offers_no_provider_retry_or_funnel_reset() {
        let message = "`aoe serve --daemon` exited: tailscale status failed".to_string();
        let mut view = ServeView {
            state: ServeViewState::Error {
                message: message.clone(),
                mode: ServeMode::Local,
                transport: None,
            },
            pending_passphrase: String::new(),
            pending_confirm: None,
            show_help: false,
            mouse: RefCell::default(),
            copied: None,
            running_persists: None,
            restart_probe: RestartProbe::default(),
            pending_launch: None,
            launch_in_flight: false,
            stop_requested: false,
            probes: Probes::new(|_| false),
        };
        for key in ['p', 'P', 'r', 'R'] {
            let action = view.handle_key(KeyEvent::from(KeyCode::Char(key)));
            assert!(matches!(action, ServeAction::Continue), "{key}");
            assert!(
                matches!(&view.state, ServeViewState::Error { message: m, mode: ServeMode::Local, .. } if *m == message),
                "{key}"
            );
        }
    }

    /// A refused or failed restart leaves the old daemon running, so the saved passphrase must
    /// stay the one that daemon accepts until `ServeLaunchRequest::run` succeeds.
    #[test]
    #[serial_test::serial]
    fn new_passphrase_is_saved_only_after_the_restart_succeeds() {
        let _home = crate::session::test_support::isolate_app_dir();
        save_passphrase_to_disk("running daemon phrase");
        let mut view = ServeView {
            state: ServeViewState::Active {
                mode: ServeMode::Tunnel,
                transport: Some(TunnelTransport::Cloudflare),
                urls: vec![],
                url_index: 0,
                passphrase: Some("running daemon phrase".to_string()),
                opened_at: Instant::now(),
                log_offset: 0,
            },
            pending_passphrase: String::new(),
            pending_confirm: None,
            show_help: false,
            mouse: RefCell::default(),
            copied: None,
            running_persists: None,
            restart_probe: RestartProbe::default(),
            pending_launch: None,
            launch_in_flight: false,
            stop_requested: false,
            probes: Probes::new(|_| false),
        };
        let key = KeyEvent::from(KeyCode::Char('G'));
        view.handle_key(key);
        assert!(matches!(view.handle_key(key), ServeAction::Launch(_)));
        assert_ne!(view.pending_passphrase, "running daemon phrase");
        assert_eq!(
            recall_passphrase().as_deref(),
            Some("running daemon phrase")
        );
    }

    #[test]
    #[serial_test::serial]
    fn restart_requires_an_uninterrupted_confirmation() {
        let _home = crate::session::test_support::isolate_app_dir();
        for (cancelled, expired) in [(false, false), (true, false), (false, true)] {
            let mut view = ServeView {
                state: ServeViewState::Active {
                    mode: ServeMode::Local,
                    transport: None,
                    urls: vec![ServeUrl {
                        label: None,
                        url: "http://127.0.0.1:8081/".to_string(),
                    }],
                    url_index: 0,
                    passphrase: None,
                    opened_at: Instant::now(),
                    log_offset: 0,
                },
                pending_passphrase: String::new(),
                pending_confirm: None,
                show_help: false,
                mouse: RefCell::default(),
                copied: None,
                running_persists: None,
                restart_probe: RestartProbe::default(),
                pending_launch: None,
                launch_in_flight: false,
                stop_requested: false,
                probes: Probes::new(|_| false),
            };
            let restart_key = KeyEvent::from(KeyCode::Char('R'));
            assert!(matches!(
                view.handle_key(restart_key),
                ServeAction::Continue
            ));
            assert!(matches!(&view.state, ServeViewState::Active { .. }));
            assert!(!view.launch_in_flight);
            assert!(view.pending_launch.is_none());
            assert!(matches!(
                view.pending_confirm,
                Some((PendingConfirm::Restart, _))
            ));

            if cancelled {
                assert!(matches!(
                    view.handle_key(KeyEvent::from(KeyCode::Char('x'))),
                    ServeAction::Continue
                ));
                assert!(view.pending_confirm.is_none());
            }
            let stale = Instant::now() - CONFIRM_TTL - Duration::from_secs(1);
            if expired {
                view.pending_confirm = Some((PendingConfirm::Restart, stale));
            }
            let action = view.handle_key(restart_key);
            if cancelled || expired {
                assert!(matches!(action, ServeAction::Continue));
                assert!(matches!(&view.state, ServeViewState::Active { .. }));
                assert!(!view.launch_in_flight);
                assert!(view.pending_launch.is_none());
                assert!(view.pending_confirm.is_some_and(|(action, when)| {
                    action == PendingConfirm::Restart && when > stale
                }));
            } else {
                let ServeAction::Launch(request) = action else {
                    panic!("second restart key must launch");
                };
                assert!(request.restart);
                assert_eq!(request.mode, ServeMode::Local);
                assert!(matches!(&view.state, ServeViewState::Starting { .. }));
                assert!(view.launch_in_flight);
                assert!(view.pending_confirm.is_none());
            }
        }
    }

    #[test]
    fn token_age_counts_only_under_token_auth() {
        let recorded = |auth: &str, core_only: bool| -> ServeLaunch {
            serde_json::from_value(serde_json::json!({
                "schema": 4, "pid": 1, "profile": "", "core_only": core_only,
                "host": "127.0.0.1", "port": 8080, "auth_mode": auth,
                "has_passphrase": false, "behind_proxy": false, "read_only": false,
                "remote": false, "tunnel_name": null, "tunnel_url": null,
                "no_tailscale": false,
            }))
            .unwrap()
        };
        let renews = TokenWindow::Renews;
        for (launch, want) in [
            (None, renews),
            (Some(recorded("token", false)), renews),
            (Some(recorded("none", true)), renews),
            (Some(recorded("passphrase", false)), TokenWindow::Reused),
            (Some(recorded("none", false)), TokenWindow::Reused),
        ] {
            assert_eq!(effective_token_window(launch.as_ref(), renews), want);
        }
    }

    /// The token can age out between the first and second R, so the second one confirms again
    /// when what it would do no longer matches what the user read.
    #[test]
    #[serial_test::serial]
    fn second_restart_key_reprobes_and_reconfirms_when_the_effect_changed() {
        let _home = crate::session::test_support::isolate_app_dir();
        let token = crate::session::get_app_dir().unwrap().join("serve.token");
        let mut view = ServeView::new();
        view.state = ServeViewState::Active {
            mode: ServeMode::Local,
            transport: None,
            urls: vec![ServeUrl {
                label: None,
                url: "http://127.0.0.1:8080/?token=t".to_string(),
            }],
            url_index: 0,
            passphrase: None,
            opened_at: Instant::now(),
            log_offset: 0,
        };
        let key = KeyEvent::from(KeyCode::Char('R'));
        let write_token = |age: Duration| {
            std::fs::write(&token, "t").unwrap();
            std::fs::File::options()
                .write(true)
                .open(&token)
                .unwrap()
                .set_modified(std::time::SystemTime::now() - age)
                .unwrap();
        };
        write_token(Duration::from_secs(23 * 60 * 60 + 60));
        view.handle_key(key);
        assert_eq!(view.restart_probe.local_token, TokenWindow::Boundary);
        write_token(Duration::from_secs(24 * 60 * 60 + 60));
        assert!(matches!(view.handle_key(key), ServeAction::Continue));
        assert_eq!(view.restart_probe.local_token, TokenWindow::Renews);
        assert!(matches!(
            view.pending_confirm,
            Some((PendingConfirm::Restart, _))
        ));
        assert!(view.pending_launch.is_none());
        assert!(matches!(view.handle_key(key), ServeAction::Launch(_)));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial_test::serial]
    async fn stop_key_returns_owned_action_while_launch_transaction_is_held() {
        let _home = crate::session::test_support::isolate_app_dir();
        let transaction = crate::daemon::lifecycle::Transaction::acquire()
            .await
            .unwrap();
        let mut view = ServeView::new();
        view.state = ServeViewState::Starting {
            mode: ServeMode::Local,
            transport: None,
            passphrase: None,
            started_at: Instant::now(),
            log_offset: 0,
        };
        view.launch_in_flight = true;
        let (sent, received) = tokio::sync::oneshot::channel();
        let key_thread = std::thread::spawn(move || {
            let action = view.handle_key(KeyEvent::from(KeyCode::Char('s')));
            let _ = sent.send((view, action));
        });
        let (mut view, action) = tokio::time::timeout(Duration::from_secs(2), received)
            .await
            .expect("Stop must not wait for the held launch transaction")
            .unwrap();
        assert!(matches!(action, ServeAction::Stop));
        assert!(view.stop_requested);
        let buffer = crate::tui::dialogs::test_render::draw(110, 40, |frame, theme| {
            view.render(frame, frame.area(), theme)
        });
        crate::tui::dialogs::test_render::find(&buffer, "Stopping daemon");
        assert!(matches!(
            view.handle_key(KeyEvent::from(KeyCode::Esc)),
            ServeAction::Close
        ));
        drop(transaction);
        key_thread.join().unwrap();
    }

    /// A probe handler that reports each probe it starts, then holds it until released.
    struct Gate {
        entered: std::sync::mpsc::Receiver<()>,
        release: std::sync::mpsc::Sender<bool>,
    }

    impl Gate {
        fn wait_entered(&self) {
            self.entered
                .recv_timeout(Duration::from_secs(10))
                .expect("probe never reached the worker");
        }
    }

    fn gated_view() -> (ServeView, Gate) {
        let (entered_tx, entered) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        let view = ServeView::with_probe_handler(move |_| {
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap()
        });
        (view, Gate { entered, release })
    }

    fn starting() -> ServeViewState {
        ServeViewState::Starting {
            mode: ServeMode::Local,
            transport: None,
            passphrase: None,
            started_at: Instant::now(),
            log_offset: 0,
        }
    }

    #[test]
    #[serial_test::serial]
    fn a_held_probe_at_construction_leaves_the_dialog_responsive() {
        let _home = crate::session::test_support::isolate_app_dir();
        let (mut view, gate) = gated_view();
        gate.wait_entered();

        assert!(!view.tick());
        assert!(matches!(
            view.handle_key(KeyEvent::from(KeyCode::Char('x'))),
            ServeAction::Continue
        ));
        let buffer = crate::tui::dialogs::test_render::draw(110, 40, |frame, theme| {
            view.render(frame, frame.area(), theme)
        });
        crate::tui::dialogs::test_render::find(&buffer, "Checking");
        assert!(matches!(
            view.handle_key(KeyEvent::from(KeyCode::Esc)),
            ServeAction::Close
        ));

        gate.release.send(true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while matches!(view.state, ServeViewState::Checking { .. }) {
            view.tick();
            assert!(Instant::now() < deadline, "probe result never applied");
            std::thread::yield_now();
        }
    }

    #[test]
    #[serial_test::serial]
    fn a_held_probe_leaves_a_clickable_close_control() {
        let _home = crate::session::test_support::isolate_app_dir();
        let (mut view, gate) = gated_view();
        gate.wait_entered();
        let buffer = crate::tui::dialogs::test_render::draw(110, 40, |frame, theme| {
            view.render(frame, frame.area(), theme)
        });
        let (col, row) = crate::tui::dialogs::test_render::find(&buffer, "[Esc] cancel");
        let key = view.handle_click(col, row).expect("close hint is a target");
        assert!(matches!(view.handle_key(key), ServeAction::Close));
        gate.release.send(true).unwrap();
    }

    #[test]
    #[serial_test::serial]
    fn a_starting_dialog_keeps_one_probe_in_flight_and_ignores_a_stale_result() {
        let _home = crate::session::test_support::isolate_app_dir();
        let (mut view, gate) = gated_view();
        gate.wait_entered();
        gate.release.send(true).unwrap();
        // The construction probe's result no longer matches once the state is replaced.
        view.state = starting();
        view.tick();
        gate.wait_entered();
        let current = view.probes.liveness.expect("a liveness probe is in flight");
        // The blocked FIFO worker hides queued requests, so count enqueues instead.
        let enqueued = view.probes.last_attempt;
        for _ in 0..5 {
            view.tick();
        }
        assert_eq!(
            view.probes.last_attempt, enqueued,
            "a second probe was queued"
        );

        // A result for an attempt the state no longer awaits must not change it.
        assert!(!view.apply_probe(current - 1, false));
        assert!(matches!(view.state, ServeViewState::Starting { .. }));
        assert_eq!(view.probes.liveness, Some(current));
        gate.release.send(true).unwrap();
    }

    #[test]
    #[serial_test::serial]
    fn a_liveness_probe_that_saw_the_daemon_exit_errors_only_without_a_published_url() {
        let _home = crate::session::test_support::isolate_app_dir();
        let mut view = ServeView::with_probe_handler(|_| false);
        view.state = starting();
        view.probes.liveness = Some(7);
        assert!(view.apply_probe(7, false));
        assert!(matches!(view.state, ServeViewState::Error { .. }));
    }
}
