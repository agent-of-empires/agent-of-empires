//! Remote access view: a controller for the `aoe serve --daemon` lifecycle, which outlives the TUI.

mod daemon;
mod jobs;
mod render;
mod words;
mod work;

use std::cell::RefCell;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;

use crate::acp::client::autostart::{initial_log_tail, LogContext};
use crate::cli::serve::{ServeLaunch, ServeUrl, TuiControlLease, TuiControlTarget};
use crate::tui::components::hover::HoverState;
use crate::tui::dialogs::{contains, hit, target_rects};
use crate::tui::styles::Theme;
use daemon::*;
pub(crate) use jobs::ServeJobs;
use jobs::{Submitter, Ticket};
use work::{run_job, Job, JobResult};

const FLASH_TTL: Duration = Duration::from_millis(1500);
const CONFIRM_TTL: Duration = Duration::from_secs(3);

pub enum ServeAction {
    Launch(ServeLaunchRequest),
    Stop(ServeStopRequest),
    Continue,
    Close,
}
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ServeStopRequest {
    target: Option<TuiControlTarget>,
}

impl ServeStopRequest {
    pub(crate) async fn run(self) -> anyhow::Result<()> {
        match self.target {
            Some(target) => {
                let cancel = tokio_util::sync::CancellationToken::new();
                let lease = TuiControlLease::prepare(target, &cancel).await?;
                lease.stop().await
            }
            None => crate::cli::serve::stop_daemon().await,
        }
    }
}

pub struct ServeLaunchRequest {
    restart: bool,
    mode: ServeMode,
    transport: Option<TunnelTransport>,
    passphrase: Option<String>,
}

#[derive(Clone)]
pub struct ServeLaunchFailure {
    pub(crate) message: String,
    pub(crate) log_tail: LogContext,
}

impl ServeLaunchFailure {
    /// Capture on the launch owner, not on the event loop.
    pub(crate) fn capture(message: String) -> Self {
        Self {
            message,
            log_tail: initial_log_tail(),
        }
    }
}

/// A dashboard launch the App runs, kept on Home so a view opened meanwhile shows it.
#[derive(Clone)]
pub enum DashboardLaunch {
    Running {
        target: LaunchTarget,
        started_at: Instant,
    },
    /// Failed with no view open; the next view shows it, once.
    Failed {
        target: LaunchTarget,
        failure: ServeLaunchFailure,
    },
}

#[cfg(test)]
impl DashboardLaunch {
    pub(crate) fn tunnel_for_test(started_at: Instant) -> Self {
        DashboardLaunch::Running {
            target: LaunchTarget {
                mode: ServeMode::Tunnel,
                transport: Some(TunnelTransport::Tailscale),
                passphrase: None,
            },
            started_at,
        }
    }
}

#[derive(Clone)]
pub struct LaunchTarget {
    mode: ServeMode,
    transport: Option<TunnelTransport>,
    passphrase: Option<String>,
}

impl ServeLaunchRequest {
    pub(crate) fn target(&self) -> LaunchTarget {
        LaunchTarget {
            mode: self.mode,
            transport: self.transport,
            passphrase: self.passphrase.clone(),
        }
    }

    pub(crate) async fn run(
        self,
        profile: String,
        predecessor_epoch: Option<String>,
    ) -> Result<crate::cli::serve::TuiDashboardLaunch, ServeLaunchFailure> {
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
        let parsed = crate::cli::Cli::try_parse_from(argv).map_err(|error| ServeLaunchFailure {
            message: error.to_string(),
            log_tail: LogContext::default(),
        })?;
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
        let result = result.map_err(ServeLaunchFailure::capture);
        if result.is_ok() {
            remember_last_mode(self.mode);
        }
        // A failed reconnect can follow a successor that already runs the new passphrase.
        if let Some(passphrase) = self.passphrase {
            if result.is_ok() || crate::cli::serve::daemon_runs_passphrase(&passphrase) {
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

    /// How a running Daemon is reachable, from the `serve.mode` token it writes. A Tunnel's
    /// token names its provider ("tailscale", or "tunnel" for Cloudflare).
    fn reachability(s: &str) -> Option<&'static str> {
        match s.trim() {
            "tailscale" => Some("Tailscale"),
            "tunnel" => Some("Cloudflare"),
            "local" => Some("Local network"),
            _ => None,
        }
    }
}

/// How the running Web dashboard is reachable, for the Connections hub.
pub fn served_via() -> Option<&'static str> {
    ServeMode::reachability(&read_app_file("serve.mode")?)
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
    fn mode_picker(cancel: &tokio_util::sync::CancellationToken) -> (Self, String) {
        let tunnel_available = crate::server::tunnel::tailscale_available_sync(cancel)
            || crate::server::tunnel::check_cloudflared(cancel).is_ok();
        let ips = crate::server::discover_tagged_ips();
        let local_available = !ips.is_empty();
        let primary = ips
            .first()
            .map(|(kind, ip)| match kind {
                crate::server::IpKind::Tailscale => format!("{ip} (Tailscale)"),
                crate::server::IpKind::Lan => format!("{ip} (LAN)"),
                crate::server::IpKind::Loopback => format!("{ip} (loopback)"),
            })
            .unwrap_or_else(|| "only localhost available".to_owned());
        // Prefer the last launched mode, falling back to whichever one is available.
        let last = read_last_mode().unwrap_or(ServeMode::Local);
        let selected = match (last, local_available, tunnel_available) {
            (ServeMode::Local, false, true) | (ServeMode::Tunnel, _, true) => ServeMode::Tunnel,
            _ => ServeMode::Local,
        };
        (
            ServeViewState::ModePicker {
                selected,
                tunnel_available,
                local_available,
                flash: None,
            },
            primary,
        )
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

    fn start_failed_with(
        mode: ServeMode,
        transport: Option<TunnelTransport>,
        headline: &str,
        tail: LogContext,
    ) -> Self {
        ServeViewState::Error {
            message: format!("{headline}{}", last_log_lines(&tail)),
            mode,
            transport,
        }
    }

    /// Active once the daemon has published its URLs, Starting until then.
    fn running(
        mode: ServeMode,
        transport: Option<TunnelTransport>,
        passphrase: Option<String>,
        urls: Vec<ServeUrl>,
        log_offset: u64,
    ) -> Self {
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
    fn now(
        mode: ServeMode,
        transport: Option<TunnelTransport>,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Self {
        let recorded = crate::cli::serve::recorded_launch();
        match mode {
            ServeMode::Local => RestartProbe {
                on_funnel: false,
                persists: persist_sessions(),
                port: crate::acp::client::autostart::saved_port(),
                local_hosts: crate::server::local_published_hosts()
                    .into_iter()
                    .map(|(_, host)| host)
                    .collect(),
                local_passphrase: std::env::var("AOE_SERVE_PASSPHRASE")
                    .is_ok_and(|pp| !pp.is_empty()),
                local_token: effective_token_window(recorded.as_ref(), token_window()),
                named_host: None,
            },
            ServeMode::Tunnel => RestartProbe {
                on_funnel: restart_on_funnel(transport, cancel),
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

/// The server's choice: `--no-tailscale` for an explicit Cloudflare pick, otherwise Funnel
/// whenever `tailscale` is installed and logged in.
fn restart_on_funnel(
    transport: Option<TunnelTransport>,
    cancel: &tokio_util::sync::CancellationToken,
) -> bool {
    let no_tailscale = transport == Some(TunnelTransport::Cloudflare);
    !crate::server::tunnel::skips_tailscale(no_tailscale, None, None)
        && crate::server::tunnel::tailscale_available_sync(cancel)
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

enum RestartIntent {
    Confirm(PendingConfirm),
    Help,
}

enum PendingJob {
    Open,
    Passphrase,
    ErrorContext,
    RetryContext {
        failed: Option<TunnelTransport>,
    },
    ResetContext,
    Picker {
        selected: Option<ServeMode>,
    },
    Providers {
        selected: Option<TunnelTransport>,
        failed: Option<TunnelTransport>,
        refreshed: bool,
    },
    Retry {
        failed: Option<TunnelTransport>,
        target: TuiControlTarget,
    },
    Reset,
    Restart {
        intent: RestartIntent,
        confirmed: Option<PendingConfirm>,
    },
    LaunchContext(ServeLaunchRequest),
    LaunchOutcome,
    Starting,
    ActiveLog,
    Copy,
}

impl PendingJob {
    fn banner(&self) -> Option<&'static str> {
        match self {
            Self::Open => Some("Looking for a running daemon"),
            Self::RetryContext { .. } => Some("Checking Daemon identity for provider retry"),
            Self::ResetContext => Some("Checking Daemon identity for Funnel reset"),
            Self::Picker { .. } => Some("Checking access options"),
            Self::Providers { .. } => Some("Checking tunnel providers"),
            Self::Retry { .. } => Some("Stopping the observed daemon before provider retry"),
            Self::Reset => Some("Resetting Funnel; closing does not undo an admitted reset"),
            Self::Restart { .. } => Some("Checking restart effects"),
            _ => None,
        }
    }
}

struct PendingWork {
    attempt: u64,
    kind: PendingJob,
    mode: ServeMode,
    transport: Option<TunnelTransport>,
    ticket: Ticket,
}

pub struct ServeView {
    state: ServeViewState,
    pending_passphrase: String,
    pending_confirm: Option<(PendingConfirm, Instant)>,
    show_help: bool,
    mouse: RefCell<ServeMouse>,
    copied: Option<(&'static str, Instant)>,
    running_persists: Option<bool>,
    restart_probe: RestartProbe,
    pending_launch: Option<ServeLaunchRequest>,
    launch_in_flight: bool,
    stop_requested: bool,
    submitter: Submitter,
    pending: Option<PendingWork>,
    last_attempt: u64,
    target: Option<crate::cli::serve::TuiControlTarget>,
    control_error: Option<String>,
    local_primary: String,
    elapsed_second: Option<u64>,
}

impl ServeView {
    pub(crate) fn new(jobs: &mut ServeJobs) -> Self {
        let mut view = Self::with_state(ServeViewState::Checking { attempt: 0 }, jobs.submitter());
        view.request(Job::Open, PendingJob::Open);
        view.state = ServeViewState::Checking {
            attempt: view.last_attempt,
        };
        view
    }

    #[cfg(test)]
    pub(crate) fn settled(jobs: &mut ServeJobs) -> Self {
        let mut view = Self::new(jobs);
        let deadline = Instant::now() + Duration::from_secs(10);
        while matches!(view.state, ServeViewState::Checking { .. }) {
            view.tick();
            assert!(Instant::now() < deadline, "Serve view never settled");
            std::thread::yield_now();
        }
        view
    }

    pub(crate) fn for_launch(launch: DashboardLaunch, jobs: &mut ServeJobs) -> Self {
        let (state, running) = match launch {
            DashboardLaunch::Running { target, started_at } => (
                ServeViewState::Starting {
                    mode: target.mode,
                    transport: target.transport,
                    passphrase: target.passphrase,
                    started_at,
                    log_offset: 0,
                },
                true,
            ),
            DashboardLaunch::Failed { target, failure } => (
                ServeViewState::start_failed_with(
                    target.mode,
                    target.transport,
                    &failure.message,
                    failure.log_tail,
                ),
                false,
            ),
        };
        let mut view = Self::with_state(state, jobs.submitter());
        view.launch_in_flight = running;
        if running {
            view.request(Job::Passphrase, PendingJob::Passphrase);
        } else {
            view.request(Job::ErrorContext, PendingJob::ErrorContext);
        }
        view
    }

    fn with_state(state: ServeViewState, submitter: Submitter) -> Self {
        Self {
            state,
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
            submitter,
            pending: None,
            last_attempt: 0,
            target: None,
            control_error: None,
            local_primary: String::new(),
            elapsed_second: None,
        }
    }

    #[cfg(test)]
    fn for_test(state: ServeViewState) -> Self {
        Self::with_state(state, Submitter::closed_for_test())
    }

    fn context(&self) -> (ServeMode, Option<TunnelTransport>) {
        match &self.state {
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
        }
    }

    fn request(&mut self, job: Job, kind: PendingJob) {
        self.pending = None;
        self.last_attempt += 1;
        let (mode, transport) = self.context();
        self.pending = Some(PendingWork {
            attempt: self.last_attempt,
            kind,
            mode,
            transport,
            ticket: self.submitter.request(self.last_attempt, job),
        });
    }

    fn request_picker(&mut self, selected: Option<ServeMode>) {
        self.request(Job::Picker, PendingJob::Picker { selected });
        self.state = ServeViewState::Checking {
            attempt: self.last_attempt,
        };
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
            self.request(Job::Copy(label, value), PendingJob::Copy);
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
        if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) && !self.show_help {
            self.pending = None;
            self.pending_confirm = None;
            self.pending_launch = None;
            return ServeAction::Close;
        }
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
            let target = match self.pending.take() {
                Some(PendingWork {
                    kind: PendingJob::Retry { target, .. },
                    ..
                }) => {
                    self.target = Some(target.clone());
                    Some(target)
                }
                _ => None,
            };
            self.begin_stop();
            return ServeAction::Stop(ServeStopRequest { target });
        }
        if self.stop_requested {
            return if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
                ServeAction::Close
            } else {
                ServeAction::Continue
            };
        }
        if self.handle_pending_key(key.code) {
            return ServeAction::Continue;
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

    fn handle_pending_key(&mut self, code: KeyCode) -> bool {
        let Some(pending) = &self.pending else {
            return false;
        };
        match &pending.kind {
            PendingJob::Providers {
                selected, failed, ..
            } => {
                let (selected, failed) = (*selected, *failed);
                match code {
                    KeyCode::Char('r' | 'R') => self.request(
                        Job::Providers,
                        PendingJob::Providers {
                            selected,
                            failed,
                            refreshed: true,
                        },
                    ),
                    KeyCode::Char('L') => self.request_picker(Some(ServeMode::Local)),
                    _ => {}
                }
                true
            }
            PendingJob::Picker { .. } => {
                let selected = match code {
                    KeyCode::Char('L') | KeyCode::Left => Some(ServeMode::Local),
                    KeyCode::Char('t' | 'T') | KeyCode::Right => Some(ServeMode::Tunnel),
                    _ => return true,
                };
                self.request_picker(selected);
                true
            }
            PendingJob::Restart { intent, .. } => {
                let repeated = match intent {
                    RestartIntent::Confirm(PendingConfirm::Restart) => {
                        matches!(code, KeyCode::Char('r' | 'R'))
                    }
                    RestartIntent::Confirm(PendingConfirm::NewPassphrase) => {
                        matches!(code, KeyCode::Char('g' | 'G'))
                    }
                    RestartIntent::Help => code == KeyCode::Char('?'),
                };
                if repeated {
                    return true;
                }
                self.pending = None;
                false
            }
            PendingJob::Open
            | PendingJob::ErrorContext
            | PendingJob::RetryContext { .. }
            | PendingJob::ResetContext
            | PendingJob::Retry { .. }
            | PendingJob::Reset
            | PendingJob::LaunchContext(_)
            | PendingJob::LaunchOutcome => true,
            PendingJob::Passphrase
            | PendingJob::Starting
            | PendingJob::ActiveLog
            | PendingJob::Copy => {
                self.pending = None;
                false
            }
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
                self.request(
                    Job::Providers,
                    PendingJob::Providers {
                        selected: None,
                        failed: None,
                        refreshed: false,
                    },
                );
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
            selected, flash, ..
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
                let selected = *selected;
                self.request(
                    Job::Providers,
                    PendingJob::Providers {
                        selected: Some(selected),
                        failed: None,
                        refreshed: true,
                    },
                );
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
        self.prepare_launch(ServeLaunchRequest {
            restart: false,
            mode,
            transport,
            passphrase,
        });
    }

    fn prepare_launch(&mut self, request: ServeLaunchRequest) {
        self.pending_confirm = None;
        self.show_help = false;
        self.launch_in_flight = true;
        self.state = ServeViewState::Starting {
            mode: request.mode,
            transport: request.transport,
            passphrase: request.passphrase.clone(),
            started_at: Instant::now(),
            log_offset: 0,
        };
        let needs_passphrase = request.mode == ServeMode::Tunnel
            && request.passphrase.as_ref().is_none_or(String::is_empty);
        self.request(
            Job::LaunchContext { needs_passphrase },
            PendingJob::LaunchContext(request),
        );
    }

    pub(crate) fn take_launch(&mut self) -> Option<ServeLaunchRequest> {
        self.pending_launch.take()
    }

    pub(crate) fn finish_launch(&mut self, result: Result<TuiControlTarget, ServeLaunchFailure>) {
        if self.stop_requested || !self.launch_in_flight {
            return;
        }
        let ServeViewState::Starting {
            mode,
            transport,
            passphrase,
            log_offset,
            ..
        } = &mut self.state
        else {
            return;
        };
        let (mode, transport) = (*mode, *transport);
        match result {
            Ok(target) => {
                self.target = Some(target.clone());
                let job = Job::LaunchOutcome {
                    target,
                    mode,
                    transport,
                    passphrase: passphrase.take(),
                    log_offset: *log_offset,
                };
                self.request(job, PendingJob::LaunchOutcome);
            }
            Err(failure) => {
                self.launch_in_flight = false;
                self.target = None;
                self.state = ServeViewState::start_failed_with(
                    mode,
                    transport,
                    &failure.message,
                    failure.log_tail,
                );
                self.request(Job::ErrorContext, PendingJob::ErrorContext);
            }
        }
    }

    pub(crate) fn begin_stop(&mut self) {
        self.pending = None;
        self.pending_confirm = None;
        self.pending_launch = None;
        self.show_help = false;
        self.stop_requested = true;
        self.launch_in_flight = true;
    }

    pub(crate) fn finish_stop(&mut self, result: Result<(), String>) {
        if !self.stop_requested {
            return;
        }
        self.stop_requested = false;
        self.launch_in_flight = false;
        let (mode, transport) = self.context();
        match result {
            Ok(()) => self.request_picker(None),
            Err(message) => {
                self.state = ServeViewState::Error {
                    message,
                    mode,
                    transport,
                };
                if self.target.is_none() {
                    self.control_error = None;
                    self.request(Job::ErrorContext, PendingJob::ErrorContext);
                }
            }
        }
    }

    fn active_key(&mut self, code: KeyCode) -> ServeAction {
        if std::mem::take(&mut self.show_help) {
            return ServeAction::Continue;
        }
        let confirmed = self
            .pending_confirm
            .take()
            .filter(|(action, when)| {
                let repeated = match action {
                    PendingConfirm::NewPassphrase => matches!(code, KeyCode::Char('g' | 'G')),
                    PendingConfirm::Restart => matches!(code, KeyCode::Char('r' | 'R')),
                };
                repeated && when.elapsed() <= CONFIRM_TTL
            })
            .map(|(action, _)| action);
        let (mode, transport) = self.context();
        let intent = match code {
            KeyCode::Char('g' | 'G') if mode == ServeMode::Tunnel => {
                Some(RestartIntent::Confirm(PendingConfirm::NewPassphrase))
            }
            KeyCode::Char('r' | 'R') => Some(RestartIntent::Confirm(PendingConfirm::Restart)),
            KeyCode::Char('?') => Some(RestartIntent::Help),
            KeyCode::Tab => {
                if let ServeViewState::Active {
                    urls, url_index, ..
                } = &mut self.state
                {
                    if urls.len() > 1 {
                        *url_index = (*url_index + 1) % urls.len();
                    }
                }
                None
            }
            _ => None,
        };
        if let Some(intent) = intent {
            self.request(
                Job::Restart(mode, transport),
                PendingJob::Restart { intent, confirmed },
            );
        }
        ServeAction::Continue
    }

    fn request_provider_retry(&mut self, failed: Option<TunnelTransport>) {
        match self.target.take() {
            Some(target) => self.request(
                Job::Retry(target.clone()),
                PendingJob::Retry { failed, target },
            ),
            None => self.request(Job::ErrorContext, PendingJob::RetryContext { failed }),
        }
    }

    fn request_funnel_reset(&mut self) {
        match &self.target {
            Some(target) => self.request(Job::Reset(target.clone()), PendingJob::Reset),
            None => self.request(Job::ErrorContext, PendingJob::ResetContext),
        }
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
        let reset = funnel_reset_applies(message, *mode, *transport);
        let failed = *transport;
        match code {
            KeyCode::Char('p' | 'P') if *mode == ServeMode::Tunnel => {
                self.request_provider_retry(failed);
            }
            KeyCode::Char('r' | 'R') if reset => self.request_funnel_reset(),
            KeyCode::Enter | KeyCode::Char(' ') => return ServeAction::Close,
            _ => {}
        }
        ServeAction::Continue
    }

    fn apply_restart(
        &mut self,
        fresh: RestartProbe,
        intent: RestartIntent,
        confirmed: Option<PendingConfirm>,
    ) {
        let ServeViewState::Active {
            mode,
            transport,
            urls,
            passphrase,
            ..
        } = &self.state
        else {
            return;
        };
        let (mode, transport) = (*mode, *transport);
        let RestartIntent::Confirm(action) = intent else {
            self.restart_probe = fresh;
            self.show_help = true;
            return;
        };
        let repeated = confirmed == Some(action);
        let unchanged = action == PendingConfirm::NewPassphrase
            || RestartEffect::of(mode, urls, &fresh, self.running_persists)
                == RestartEffect::of(mode, urls, &self.restart_probe, self.running_persists);
        if repeated && unchanged {
            let passphrase = if action == PendingConfirm::NewPassphrase {
                self.pending_passphrase = generate_passphrase();
                Some(self.pending_passphrase.clone())
            } else {
                (mode == ServeMode::Tunnel).then(|| {
                    passphrase
                        .clone()
                        .unwrap_or_else(|| self.pending_passphrase.clone())
                })
            };
            self.prepare_launch(ServeLaunchRequest {
                restart: true,
                mode,
                transport,
                passphrase,
            });
        } else {
            self.restart_probe = fresh;
            self.pending_confirm = Some((action, Instant::now()));
        }
    }

    /// Drain one bounded result and expire UI clocks. No process or filesystem work.
    pub fn tick(&mut self) -> bool {
        use std::sync::mpsc::TryRecvError;
        let mut changed = false;
        if let Some(pending) = &self.pending {
            let result = pending.ticket.try_recv();
            match result {
                Ok((attempt, result)) if attempt == pending.attempt => {
                    let pending = self.pending.take().expect("pending result");
                    changed = self.apply_result(pending, result);
                }
                Ok(_) => {}
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    let pending = self.pending.take().expect("closed pending worker");
                    self.fail_pending(
                        pending.kind,
                        pending.mode,
                        pending.transport,
                        "The background Serve worker stopped. An admitted operation may have changed state; inspect `aoe serve --status` before retrying.".to_owned(),
                    );
                    changed = true;
                }
            }
        }
        if self
            .pending_confirm
            .is_some_and(|(_, when)| when.elapsed() > CONFIRM_TTL)
        {
            self.pending_confirm = None;
            changed = true;
        }
        if self
            .copied
            .is_some_and(|(_, when)| when.elapsed() > FLASH_TTL)
        {
            self.copied = None;
            changed = true;
        }
        match &mut self.state {
            ServeViewState::ModePicker { flash, .. } | ServeViewState::Confirm { flash, .. } => {
                changed |= expire_flash(flash);
            }
            _ => {}
        }
        if self.pending.is_none() && !self.launch_in_flight && !self.stop_requested {
            match self.state {
                ServeViewState::Starting {
                    mode, transport, ..
                } => {
                    if let Some(target) = self.target.clone() {
                        self.request(Job::ObserveStarting(target), PendingJob::Starting);
                    } else {
                        self.fail_work(
                            mode,
                            transport,
                            "The starting Daemon has no verified identity; reopen Remote Access before retrying."
                                .to_owned(),
                        );
                        changed = true;
                    }
                }
                ServeViewState::Active { .. } => {
                    self.request(Job::ObserveActive, PendingJob::ActiveLog)
                }
                _ => {}
            }
        }
        let elapsed = match &self.state {
            ServeViewState::Starting { started_at, .. }
                if !self.stop_requested
                    && self
                        .pending
                        .as_ref()
                        .is_none_or(|pending| pending.kind.banner().is_none()) =>
            {
                Some(started_at.elapsed().as_secs())
            }
            _ => None,
        };
        changed |= self.elapsed_second != elapsed;
        self.elapsed_second = elapsed;
        changed
    }

    fn fail_pending(
        &mut self,
        kind: PendingJob,
        mode: ServeMode,
        transport: Option<TunnelTransport>,
        message: String,
    ) {
        if let PendingJob::Retry { target, .. } = kind {
            self.target = Some(target);
            self.fail_work(mode, transport, message);
            return;
        }
        if matches!(
            kind,
            PendingJob::ErrorContext | PendingJob::RetryContext { .. } | PendingJob::ResetContext
        ) && matches!(self.state, ServeViewState::Error { .. })
        {
            self.target = None;
            self.control_error = Some(message);
        } else {
            self.fail_work(mode, transport, message);
        }
    }

    fn fail_work(&mut self, mode: ServeMode, transport: Option<TunnelTransport>, message: String) {
        self.pending_confirm = None;
        self.pending_launch = None;
        self.launch_in_flight = false;
        self.show_help = false;
        self.control_error = None;
        self.state = ServeViewState::Error {
            message,
            mode,
            transport,
        };
    }

    fn apply_result(&mut self, pending: PendingWork, result: JobResult) -> bool {
        match (pending.kind, result) {
            (kind, JobResult::Failed(message)) => {
                self.fail_pending(kind, pending.mode, pending.transport, message)
            }
            (
                kind @ (PendingJob::Open | PendingJob::Picker { .. } | PendingJob::LaunchOutcome),
                JobResult::Snapshot(snapshot),
            ) => {
                if matches!(&kind, PendingJob::Open | PendingJob::Picker { .. })
                    && !matches!(self.state, ServeViewState::Checking { attempt } if attempt == pending.attempt)
                {
                    return false;
                }
                self.state = snapshot.state;
                if let PendingJob::Picker {
                    selected: Some(want),
                } = kind
                {
                    if let ServeViewState::ModePicker { selected, .. } = &mut self.state {
                        *selected = want;
                    }
                }
                self.target = Some(snapshot.target);
                self.control_error = None;
                if let Some(passphrase) = snapshot.passphrase {
                    self.pending_passphrase = passphrase;
                }
                if let Some(primary) = snapshot.local_primary {
                    self.local_primary = primary;
                }
                self.launch_in_flight = false;
            }
            (
                kind @ (PendingJob::ErrorContext
                | PendingJob::RetryContext { .. }
                | PendingJob::ResetContext),
                JobResult::ErrorContext(target),
            ) => {
                self.target = Some(target);
                self.control_error = None;
                match kind {
                    PendingJob::RetryContext { failed } => self.request_provider_retry(failed),
                    PendingJob::ResetContext => self.request_funnel_reset(),
                    _ => {}
                }
            }
            (PendingJob::Passphrase, JobResult::Passphrase(passphrase)) => {
                self.pending_passphrase = passphrase
            }
            (
                PendingJob::Providers {
                    selected,
                    failed,
                    refreshed,
                },
                JobResult::Providers(tailscale, cloudflare),
            ) => {
                self.state = ServeViewState::provider_retry(failed, tailscale, cloudflare);
                if let ServeViewState::Confirm {
                    selected: current,
                    flash,
                    ..
                } = &mut self.state
                {
                    if let Some(selected) = selected {
                        *current = selected;
                    }
                    if refreshed {
                        *flash = Some(("Refreshed.".to_owned(), Instant::now()));
                    }
                }
            }
            (
                PendingJob::Retry { failed, .. },
                JobResult::Retried {
                    tailscale,
                    cloudflare,
                    target,
                },
            ) => {
                self.target = Some(target);
                self.state = ServeViewState::provider_retry(failed, tailscale, cloudflare);
            }
            (PendingJob::Reset, JobResult::Reset(result)) => {
                let message = match result {
                    Ok(()) => "Ran `tailscale funnel reset`. The existing funnel config (if any) has been cleared.\n\nPress P to pick a provider and start again.".to_owned(),
                    Err(error) => format!("`tailscale funnel reset` did not complete: {error}\n\nInspect Funnel state before retrying, then press P to pick a provider."),
                };
                self.state = ServeViewState::Error {
                    message,
                    mode: pending.mode,
                    transport: pending.transport,
                };
            }
            (PendingJob::Restart { intent, confirmed }, JobResult::Restart(fresh)) => {
                self.apply_restart(fresh, intent, confirmed)
            }
            (
                PendingJob::LaunchContext(mut request),
                JobResult::LaunchContext {
                    persists,
                    log_offset,
                    passphrase,
                },
            ) => {
                if let Some(passphrase) = passphrase {
                    self.pending_passphrase = passphrase;
                    request.passphrase = Some(self.pending_passphrase.clone());
                    if let ServeViewState::Starting {
                        passphrase: current,
                        ..
                    } = &mut self.state
                    {
                        *current = request.passphrase.clone();
                    }
                }
                self.running_persists = Some(persists);
                if let ServeViewState::Starting {
                    log_offset: current,
                    ..
                } = &mut self.state
                {
                    *current = log_offset;
                }
                self.pending_launch = Some(request);
            }
            (
                PendingJob::Starting | PendingJob::LaunchOutcome,
                JobResult::Starting(observation),
            ) => {
                self.launch_in_flight = false;
                self.target = Some(observation.target);
                let ServeViewState::Starting {
                    mode,
                    transport,
                    passphrase,
                    log_offset,
                    ..
                } = &mut self.state
                else {
                    return false;
                };
                let grew = *log_offset != observation.log_offset;
                *log_offset = observation.log_offset;
                if !observation.urls.is_empty() {
                    let passphrase = passphrase.take().or_else(|| {
                        (*mode == ServeMode::Tunnel)
                            .then_some(observation.passphrase)
                            .flatten()
                    });
                    self.state = ServeViewState::Active {
                        mode: *mode,
                        transport: *transport,
                        urls: observation.urls,
                        url_index: 0,
                        passphrase,
                        opened_at: Instant::now(),
                        log_offset: observation.log_offset,
                    };
                } else if !observation.running {
                    let headline = match mode {
                        ServeMode::Tunnel => {
                            "`aoe serve --remote --daemon` exited before the tunnel came up."
                        }
                        ServeMode::Local => {
                            "`aoe serve --daemon` exited before the server started."
                        }
                    };
                    self.state = ServeViewState::start_failed_with(
                        *mode,
                        *transport,
                        headline,
                        observation.tail,
                    );
                } else {
                    return grew;
                }
            }
            (PendingJob::ActiveLog, JobResult::ActiveLog(offset)) => {
                if let ServeViewState::Active { log_offset, .. } = &mut self.state {
                    let grew = *log_offset != offset;
                    *log_offset = offset;
                    return grew;
                }
                return false;
            }
            (PendingJob::Copy, JobResult::Copied(label)) => {
                self.copied = Some((label, Instant::now()))
            }
            (kind, _) => self.fail_pending(
                kind,
                pending.mode,
                pending.transport,
                "Unexpected Serve worker result; reopen the view before retrying.".to_owned(),
            ),
        }
        true
    }

    pub fn render(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        render::render(self, frame, area, theme);
    }
}

#[cfg(test)]
mod tests;
