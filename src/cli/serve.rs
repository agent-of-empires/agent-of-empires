//! `aoe serve` command -- start a web dashboard for remote session access

use anyhow::{bail, Context, Result};
use clap::{Args, ValueEnum};
use std::path::PathBuf;
use std::sync::Mutex;

/// How the dashboard authenticates HTTP/WS requests.
///
/// `Token` is the historical default: a random URL token gates every
/// request. `Passphrase` drops the token gate but keeps the passphrase
/// login wall as the sole human gate (useful behind a reverse proxy
/// where pasting a token URL on mobile is too high friction).
/// `None` disables both, equivalent to legacy `--no-auth`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum, serde::Serialize, serde::Deserialize)]
#[value(rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum AuthMode {
    Token,
    Passphrase,
    None,
}

impl AuthMode {
    pub(crate) fn as_cli_str(self) -> &'static str {
        match self {
            AuthMode::Token => "token",
            AuthMode::Passphrase => "passphrase",
            AuthMode::None => "none",
        }
    }
}

#[derive(Args)]
pub struct ServeArgs {
    /// Port to listen on (default: 8080; debug builds default to 8081 so a
    /// `cargo run` instance does not collide with an installed release `aoe`).
    #[arg(long)]
    pub port: Option<u16>,

    /// Host/IP to bind to (use 0.0.0.0 for LAN/VPN access)
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,

    /// Authentication mode: `token` (default, random URL token),
    /// `passphrase` (no token URL, passphrase login wall only),
    /// or `none` (no auth at all, loopback-only unless --behind-proxy).
    /// Mutually exclusive with --no-auth (which aliases --auth=none).
    #[arg(long, value_enum, conflicts_with = "no_auth")]
    pub auth: Option<AuthMode>,

    /// Disable authentication (only allowed with localhost binding).
    /// Alias for --auth=none.
    #[arg(long)]
    pub no_auth: bool,

    /// Mark this server as sitting behind a reverse proxy that
    /// terminates TLS upstream. Sets cookies as `; Secure` and trusts
    /// the `X-Forwarded-For` / `cf-connecting-ip` headers from
    /// loopback peers. Does NOT auto-spawn a tunnel (unlike --remote).
    /// Required when --auth=passphrase or --auth=none is combined with
    /// a non-loopback bind.
    #[arg(long)]
    pub behind_proxy: bool,

    /// Extra `Host` header value to accept (repeatable). The DNS-rebinding
    /// gate trusts loopback, any routable IP literal (LAN/tailnet IPs can't be
    /// rebound), and a non-wildcard `--host` by default; add a HOSTNAME or mDNS
    /// name here when serving behind a reverse proxy, a custom tunnel, or by
    /// name when binding `0.0.0.0` (access by IP needs no flag). Auto-injected
    /// tunnel hosts (`--remote`) need no flag.
    #[arg(long = "allowed-host", value_name = "HOST")]
    pub allowed_host: Vec<String>,

    /// Extra browser `Origin` to accept (repeatable, full origin
    /// `scheme://host[:port]`, e.g. `https://aoe.example.com:8443`). Needed
    /// only for a reverse proxy on a nonstandard port; standard 80/443 origins
    /// for `--allowed-host` entries are derived automatically.
    #[arg(long = "allowed-origin", value_name = "ORIGIN")]
    pub allowed_origin: Vec<String>,

    /// Read-only mode: view terminals but cannot send keystrokes
    #[arg(long)]
    pub read_only: bool,

    /// CityHall client mode: a locked-down, composer-first dashboard for
    /// non-technical users (structured view only; no terminal/diff/project
    /// management). Equivalent to `AOE_CITYHALL_MODE=1`; the flag is what the
    /// daemon replays to its restart child so the mode survives `aoe update`
    /// and `aoe serve --restart`. See #7.
    #[arg(long)]
    pub cityhall: bool,

    /// Expose the daemon over a public HTTPS tunnel. Prefers Tailscale
    /// Funnel when `tailscale` is installed and logged in (stable
    /// `.ts.net` URL, installable PWAs survive restarts). Falls back to a
    /// Cloudflare quick tunnel otherwise (fresh URL on every restart).
    #[arg(long)]
    pub remote: bool,

    /// Use a named Cloudflare Tunnel (requires prior `cloudflared tunnel create`).
    /// Takes precedence over Tailscale auto-detection.
    #[arg(long, requires = "remote")]
    pub tunnel_name: Option<String>,

    /// Skip Tailscale Funnel auto-detection and go straight to Cloudflare.
    /// Useful if you have Tailscale installed for unrelated reasons.
    #[arg(long, requires = "remote")]
    pub no_tailscale: bool,

    /// Hostname for a named tunnel (e.g., aoe.example.com)
    #[arg(long, requires = "tunnel_name")]
    pub tunnel_url: Option<String>,

    /// Run as a background daemon (detach from terminal)
    #[arg(long)]
    pub daemon: bool,

    /// Stop a running daemon
    #[arg(long)]
    pub stop: bool,

    /// Print the running daemon's PID, mode, URLs, and log path. Exits
    /// non-zero when no daemon is running. Useful for shell scripts
    /// that want to know whether a daemon is up without parsing `ps`.
    ///
    /// `--status` is read-only and incompatible with every flag that
    /// would change daemon state (`--stop`, `--daemon`, `--remote`) or
    /// the bind config of a fresh daemon (`--no-auth`, `--auth`,
    /// `--behind-proxy`, `--read-only`, `--passphrase`, `--port`,
    /// `--tunnel-name`, `--no-tailscale`, `--tunnel-url`, `--open`,
    /// `--allowed-host`, `--allowed-origin`).
    /// Clap reports the misuse instead of silently ignoring the extras.
    #[arg(
        long,
        conflicts_with_all = [
            "stop", "daemon", "remote", "restart",
            "no_auth", "auth", "behind_proxy",
            "read_only", "cityhall", "passphrase", "port",
            "tunnel_name", "no_tailscale", "tunnel_url", "open",
            "allowed_host", "allowed_origin",
        ],
    )]
    pub status: bool,

    /// Require a passphrase for login (second-factor auth).
    /// Can also be set via AOE_SERVE_PASSPHRASE environment variable.
    #[arg(long, env = "AOE_SERVE_PASSPHRASE")]
    pub passphrase: Option<String>,

    /// Open the dashboard URL in the default browser once the server is ready.
    /// Ignored in a build with no dashboard bundle, under --daemon or --remote,
    /// and whenever no browser the user could see is reachable (see
    /// `tui::open_url`): over SSH without a forwarded display, or on Linux/BSD
    /// with no display server. `BROWSER` overrides the check on platforms whose
    /// launcher reads it, which excludes macOS.
    #[arg(long)]
    pub open: bool,

    /// Internal marker: this invocation is the detached child spawned by
    /// `--daemon`. Set automatically by `start_daemon()`; never pass by hand.
    /// Tells `main.rs` to classify the process as `ServeDaemonChild` so the
    /// sink resolver routes tracing to the configured log file (its
    /// stdout/stderr are detached). Hidden from `--help`.
    #[arg(long, hide = true)]
    pub daemon_child: bool,

    /// Restart a running `aoe serve` daemon, replaying the host, port,
    /// mode, and auth it was launched with (read from `serve.launch`).
    /// The passphrase is recalled from `serve.passphrase` or
    /// `AOE_SERVE_PASSPHRASE` before the old daemon is stopped, so a
    /// passphrase-protected daemon is never left down.
    /// Incompatible with the flags that would change the daemon's bind
    /// config: that config comes from the persisted launch state.
    #[arg(
        long,
        conflicts_with_all = [
            "stop", "daemon", "remote",
            "no_auth", "auth", "behind_proxy",
            "read_only", "cityhall", "passphrase", "port", "host",
            "tunnel_name", "no_tailscale", "tunnel_url", "open",
            "allowed_host", "allowed_origin",
        ],
    )]
    pub restart: bool,
}

impl ServeArgs {
    pub fn resolved_port(&self) -> u16 {
        self.port
            .unwrap_or(if cfg!(debug_assertions) { 8081 } else { 8080 })
    }
}

fn host_is_localhost(host: &str) -> bool {
    host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

fn resolve_auth_mode(auth: Option<AuthMode>, no_auth: bool) -> AuthMode {
    match (auth, no_auth) {
        (Some(mode), false) => mode,
        (None, true) => AuthMode::None,
        (None, false) => AuthMode::Token,
        (Some(_), true) => unreachable!("clap conflicts_with prevents this"),
    }
}

fn validate_auth_combination(
    auth_mode: AuthMode,
    has_passphrase: bool,
    is_localhost: bool,
    behind_proxy: bool,
    remote: bool,
    host: &str,
) -> Result<()> {
    if matches!(auth_mode, AuthMode::Passphrase) && !has_passphrase {
        bail!(
            "--auth=passphrase requires --passphrase <VALUE> or AOE_SERVE_PASSPHRASE.\n\
             Without a passphrase there is no gate. Use --auth=none if that is intended."
        );
    }

    if matches!(auth_mode, AuthMode::None) && has_passphrase {
        bail!("--auth=none does not honor --passphrase; use --auth=passphrase instead.");
    }

    if matches!(auth_mode, AuthMode::None | AuthMode::Passphrase) && !is_localhost && !behind_proxy
    {
        bail!(
            "Refusing to start with --auth={} on {}.\n\
             Reduced-auth modes on a non-loopback bind require --behind-proxy,\n\
             which signals that an upstream reverse proxy terminates TLS and\n\
             forwards the client IP via X-Forwarded-For / cf-connecting-ip.",
            auth_mode.as_cli_str(),
            host
        );
    }

    if matches!(auth_mode, AuthMode::None | AuthMode::Passphrase) && remote {
        bail!(
            "Refusing to start with --auth={} in remote mode.\n\
             --remote exposes the dashboard to the public internet and requires\n\
             both token auth and a passphrase. If you have an external reverse\n\
             proxy, use --behind-proxy instead of --remote.",
            auth_mode.as_cli_str()
        );
    }

    Ok(())
}

fn validate_behind_proxy_allowlist(
    behind_proxy: bool,
    remote: bool,
    allowed_hosts: &[String],
) -> Result<()> {
    if behind_proxy && !remote && allowed_hosts.is_empty() {
        bail!(
            "--behind-proxy requires --allowed-host <public-hostname>.\n\
             The reverse proxy forwards requests carrying your public Host header,\n\
             which aoe cannot infer. Without it the DNS-rebinding gate would reject\n\
             every proxied request. Example:\n  \
             aoe serve --host 127.0.0.1 --behind-proxy --allowed-host aoe.example.com"
        );
    }
    Ok(())
}

const FORBIDDEN_AUTHORITY_CHARS: [char; 4] = ['/', '?', '#', '@'];

fn validate_allowed_origins(allowed_origins: &[String]) -> Result<()> {
    for origin in allowed_origins {
        let lower = origin.trim().to_ascii_lowercase();
        let host = lower
            .strip_prefix("https://")
            .or_else(|| lower.strip_prefix("http://"));
        let valid = host.is_some_and(|h| {
            let h = h.trim_end_matches('/');
            !h.contains(FORBIDDEN_AUTHORITY_CHARS) && !crate::server::norm_host(h).is_empty()
        });
        if !valid {
            bail!(
                "--allowed-origin {origin:?} must be a full origin of the form \
                 scheme://host[:port].\n\
                 A browser Origin always carries a scheme and a host and no path, \
                 query, or userinfo, so any other value can never match and would \
                 reject the requests it should allow. Example:\n  \
                 aoe serve --allowed-origin https://aoe.example.com:8443"
            );
        }
        if let Some(h) = host {
            if crate::server::is_untrusted_ip_literal(&crate::server::norm_host(
                h.trim_end_matches('/'),
            )) {
                bail!(
                    "--allowed-origin {origin:?} resolves to an unspecified \
                     (0.0.0.0, ::), link-local, or multicast host the DNS-rebinding \
                     gate never trusts.\n\
                     Browsers can send an IP-literal Origin (e.g. http://0.0.0.0), so \
                     allowlisting one would reopen the hole the gate closes. Use the \
                     machine's routable hostname or IP instead. Example:\n  \
                     aoe serve --allowed-origin https://aoe.example.com:8443"
                );
            }
        }
    }
    Ok(())
}

fn validate_allowed_hosts(allowed_hosts: &[String]) -> Result<()> {
    for host in allowed_hosts {
        let trimmed = host.trim();
        let normalized = crate::server::norm_host(trimmed);
        if trimmed.contains(FORBIDDEN_AUTHORITY_CHARS) || normalized.is_empty() {
            bail!(
                "--allowed-host {host:?} must be a bare hostname or IP \
                 (optionally host:port), without a scheme, path, query, or \
                 userinfo.\n\
                 The DNS-rebinding gate compares it against the request's Host \
                 header, so a value that carries any of those or normalizes to \
                 nothing (e.g. \":8080\") can never match and would reject the \
                 requests it should allow. Example:\n  \
                 aoe serve --host 0.0.0.0 --allowed-host aoe.example.com"
            );
        }
        if crate::server::is_untrusted_ip_literal(&normalized) {
            bail!(
                "--allowed-host {host:?} is an unspecified (0.0.0.0, ::), \
                 link-local, or multicast address the DNS-rebinding gate never \
                 trusts.\n\
                 A wildcard bind means \"all interfaces\", not a name a client \
                 sends, and link-local reaches cloud metadata (169.254.169.254), \
                 so allowlisting one would reopen the hole the gate closes. Reach \
                 a wildcard-bound server by its routable LAN or tailnet IP, which \
                 needs no --allowed-host, or allow its hostname. Example:\n  \
                 aoe serve --host 0.0.0.0 --allowed-host aoe.example.com"
            );
        }
    }
    Ok(())
}

fn cloudflared_required(
    no_tailscale: bool,
    has_tunnel_name: bool,
    tailscale_available: bool,
) -> bool {
    no_tailscale || has_tunnel_name || !tailscale_available
}

pub fn pid_file_path() -> Result<PathBuf> {
    let dir = crate::session::get_app_dir()?;
    Ok(dir.join("serve.pid"))
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ServeLaunch {
    pub schema: u32,
    pub pid: u32,
    #[serde(default)]
    pub instance_id: Option<String>,
    pub profile: String,
    pub host: String,
    pub port: u16,
    pub auth_mode: AuthMode,
    pub behind_proxy: bool,
    pub read_only: bool,
    #[serde(default)]
    pub cityhall: bool,
    pub remote: bool,
    pub tunnel_name: Option<String>,
    pub tunnel_url: Option<String>,
    pub no_tailscale: bool,
    #[serde(default)]
    pub allowed_host: Vec<String>,
    #[serde(default)]
    pub allowed_origin: Vec<String>,
}

const SERVE_LAUNCH_SCHEMA: u32 = 2;
const SERVE_INSTANCE_ENV: &str = "AOE_SERVE_INSTANCE_ID";

impl ServeLaunch {
    fn to_serve_args(&self, passphrase: Option<String>) -> ServeArgs {
        ServeArgs {
            port: Some(self.port),
            host: self.host.clone(),
            auth: Some(self.auth_mode),
            no_auth: false,
            behind_proxy: self.behind_proxy,
            read_only: self.read_only,
            cityhall: self.cityhall,
            remote: self.remote,
            tunnel_name: self.tunnel_name.clone(),
            no_tailscale: self.no_tailscale,
            tunnel_url: self.tunnel_url.clone(),
            daemon: true,
            stop: false,
            status: false,
            passphrase,
            open: false,
            daemon_child: false,
            restart: false,
            allowed_host: self.allowed_host.clone(),
            allowed_origin: self.allowed_origin.clone(),
        }
    }
}

fn launch_needs_passphrase(launch: &ServeLaunch) -> bool {
    launch.remote || matches!(launch.auth_mode, AuthMode::Passphrase)
}

fn serve_launch_path() -> Result<PathBuf> {
    let dir = crate::session::get_app_dir()?;
    Ok(dir.join("serve.launch"))
}

fn write_serve_launch(state: &ServeLaunch) -> Result<()> {
    let path = serve_launch_path()?;
    let json = serde_json::to_string_pretty(state)?;
    std::fs::write(&path, json).with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

fn read_serve_launch() -> Result<ServeLaunch> {
    let path = serve_launch_path()?;
    let raw =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
}

fn launch_authorizes(launch: &ServeLaunch, pid: u32, instance_is_live: bool) -> bool {
    launch.pid == pid && !launch_contradicts(launch, pid, instance_is_live)
}

pub(crate) fn serve_launch_matches(pid: u32) -> bool {
    read_serve_launch()
        .is_ok_and(|launch| launch_authorizes(&launch, pid, instance_is_live(&launch, pid)))
}

fn instance_is_live(launch: &ServeLaunch, pid: u32) -> bool {
    launch
        .instance_id
        .as_deref()
        .is_some_and(|instance_id| live_daemon_instance_matches(pid, instance_id))
}

fn launch_contradicts(launch: &ServeLaunch, pid: u32, instance_is_live: bool) -> bool {
    launch.pid == pid && launch.instance_id.is_some() && !instance_is_live
}

fn serve_launch_contradicts(pid: u32) -> bool {
    read_serve_launch()
        .is_ok_and(|launch| launch_contradicts(&launch, pid, instance_is_live(&launch, pid)))
}

fn environment_has_daemon_instance(environment: &[u8], instance_id: &str) -> bool {
    let expected = format!("{SERVE_INSTANCE_ENV}={instance_id}");
    if environment.contains(&0) {
        environment
            .split(|byte| *byte == 0)
            .any(|entry| entry == expected.as_bytes())
    } else {
        environment
            .split(|byte| byte.is_ascii_whitespace())
            .any(|entry| entry == expected.as_bytes())
    }
}

fn live_daemon_instance_matches(pid: u32, instance_id: &str) -> bool {
    let proc_path = format!("/proc/{pid}/environ");
    if std::path::Path::new(&proc_path).exists() {
        return std::fs::read(proc_path)
            .ok()
            .is_some_and(|environment| environment_has_daemon_instance(&environment, instance_id));
    }

    std::process::Command::new("ps")
        .args(["-ww", "-E", "-o", "command=", "-p", &pid.to_string()])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .is_some_and(|output| environment_has_daemon_instance(&output.stdout, instance_id))
}

fn recall_serve_passphrase() -> Option<String> {
    if let Ok(dir) = crate::session::get_app_dir() {
        if let Ok(raw) = std::fs::read_to_string(dir.join("serve.passphrase")) {
            let trimmed = raw.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    if let Ok(p) = std::env::var("AOE_SERVE_PASSPHRASE") {
        if !p.is_empty() {
            return Some(p);
        }
    }
    None
}

#[derive(Debug, Clone)]
pub struct ServeUrl {
    pub label: Option<String>,
    pub url: String,
}

pub fn read_serve_urls() -> Vec<ServeUrl> {
    let Ok(dir) = crate::session::get_app_dir() else {
        return Vec::new();
    };
    let Ok(raw) = std::fs::read_to_string(dir.join("serve.url")) else {
        return Vec::new();
    };
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

pub fn cached_serve_mode_label() -> Option<&'static str> {
    static CACHE: Mutex<Option<(u32, Option<&'static str>)>> = Mutex::new(None);

    let pid = daemon_pid()?;
    if let Ok(mut guard) = CACHE.lock() {
        if let Some((cached_pid, cached_label)) = *guard {
            if cached_pid == pid {
                return cached_label;
            }
        }
        let label = read_serve_mode_label();
        *guard = Some((pid, label));
        label
    } else {
        read_serve_mode_label()
    }
}

fn read_serve_mode_label() -> Option<&'static str> {
    let dir = crate::session::get_app_dir().ok()?;
    let raw = std::fs::read_to_string(dir.join("serve.mode")).ok()?;
    match raw.trim() {
        "local" => Some("local"),
        "tunnel" => Some("tunnel"),
        "tailscale" => Some("tailscale"),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DaemonProcessIdentity {
    Verified,
    Foreign,
    Indeterminate,
}

fn command_is_aoe_serve(command: &[u8]) -> bool {
    let args: Vec<&[u8]> = if command.contains(&0) {
        command
            .split(|byte| *byte == 0)
            .filter(|arg| !arg.is_empty())
            .collect()
    } else {
        command
            .split(|byte| byte.is_ascii_whitespace())
            .filter(|arg| !arg.is_empty())
            .collect()
    };
    let Some(executable) = args.first() else {
        return false;
    };
    let basename = executable
        .rsplit(|byte| *byte == b'/')
        .next()
        .unwrap_or(executable);
    if !matches!(basename, b"aoe" | b"agent-of-empires") {
        return false;
    }

    let mut index = 1;
    while let Some(arg) = args.get(index) {
        match *arg {
            b"-p" | b"--profile" | b"--daemon-url" => {
                index += 2;
                if index > args.len() {
                    return false;
                }
            }
            arg if arg.starts_with(b"--profile=")
                || arg.starts_with(b"--daemon-url=")
                || (arg.starts_with(b"-p") && arg.len() > 2) =>
            {
                index += 1;
            }
            b"serve" => return true,
            _ => return false,
        }
    }
    false
}

fn inspect_daemon_process(pid: i32) -> DaemonProcessIdentity {
    let proc_path = format!("/proc/{}/cmdline", pid);
    if std::path::Path::new(&proc_path).exists() {
        return match std::fs::read(&proc_path) {
            Ok(cmdline) if command_is_aoe_serve(&cmdline) => DaemonProcessIdentity::Verified,
            Ok(_) => DaemonProcessIdentity::Foreign,
            Err(_) => DaemonProcessIdentity::Indeterminate,
        };
    }

    match std::process::Command::new("ps")
        .args(["-ww", "-o", "command=", "-p", &pid.to_string()])
        .output()
    {
        Ok(out) if !out.status.success() => DaemonProcessIdentity::Indeterminate,
        Ok(out) if command_is_aoe_serve(&out.stdout) => DaemonProcessIdentity::Verified,
        Ok(out) if command_mentions_aoe_executable(&out.stdout) => {
            DaemonProcessIdentity::Indeterminate
        }
        Ok(_) => DaemonProcessIdentity::Foreign,
        Err(_) => DaemonProcessIdentity::Indeterminate,
    }
}

fn command_mentions_aoe_executable(command: &[u8]) -> bool {
    command.windows(4).any(|window| window == b"aoe ")
        || command
            .windows(17)
            .any(|window| window == b"agent-of-empires ")
}

fn parse_positive_pid(raw: &str) -> Option<i32> {
    raw.trim().parse().ok().filter(|pid| *pid > 0)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DaemonProbeDisposition {
    VerifyIdentity,
    Stale,
    Indeterminate,
}

fn classify_daemon_probe(
    result: std::result::Result<(), nix::errno::Errno>,
) -> DaemonProbeDisposition {
    match result {
        Ok(()) => DaemonProbeDisposition::VerifyIdentity,
        Err(nix::errno::Errno::ESRCH) => DaemonProbeDisposition::Stale,
        Err(_) => DaemonProbeDisposition::Indeterminate,
    }
}

/// Files `aoe serve --daemon` leaves beside its pid file.
const SERVE_STATE_FILES: [&str; 4] = [
    "serve.url",
    "serve.mode",
    "serve.passphrase",
    "serve.launch",
];

fn remove_stale_serve_state(pid_path: &std::path::Path) {
    let _ = std::fs::remove_file(pid_path);
    if let Ok(dir) = crate::session::get_app_dir() {
        for name in SERVE_STATE_FILES {
            let _ = std::fs::remove_file(dir.join(name));
        }
    }
}

async fn clear_serve_state() {
    if let Ok(dir) = crate::session::get_app_dir() {
        for name in SERVE_STATE_FILES {
            let _ = tokio::fs::remove_file(dir.join(name)).await;
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DaemonStatus {
    Absent,
    Verified(u32),
    Unverified,
}

pub(crate) fn daemon_status() -> DaemonStatus {
    let Ok(path) = pid_file_path() else {
        return DaemonStatus::Unverified;
    };
    let pid_str = match std::fs::read_to_string(&path) {
        Ok(pid) => pid,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return DaemonStatus::Absent,
        Err(_) => return DaemonStatus::Unverified,
    };
    let Some(pid) = parse_positive_pid(&pid_str) else {
        return DaemonStatus::Unverified;
    };

    let probe = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None);
    match classify_daemon_probe(probe) {
        DaemonProbeDisposition::VerifyIdentity => match inspect_daemon_process(pid) {
            DaemonProcessIdentity::Verified => DaemonStatus::Verified(pid as u32),
            DaemonProcessIdentity::Foreign => {
                remove_stale_serve_state(&path);
                DaemonStatus::Absent
            }
            DaemonProcessIdentity::Indeterminate => DaemonStatus::Unverified,
        },
        DaemonProbeDisposition::Stale => {
            remove_stale_serve_state(&path);
            DaemonStatus::Absent
        }
        DaemonProbeDisposition::Indeterminate => DaemonStatus::Unverified,
    }
}

pub fn daemon_pid() -> Option<u32> {
    match daemon_status() {
        DaemonStatus::Verified(pid) => Some(pid),
        DaemonStatus::Absent | DaemonStatus::Unverified => None,
    }
}

#[tracing::instrument(target = "cli.serve", skip_all, fields(profile = %profile))]
pub async fn run(profile: &str, mut args: ServeArgs) -> Result<()> {
    if args.stop {
        return stop_daemon().await;
    }

    if args.status {
        return print_status().await;
    }

    if args.restart {
        return restart_daemon().await;
    }

    crate::session::require_known_profile(profile)?;

    args.cityhall = args.cityhall || std::env::var_os("AOE_CITYHALL_MODE").is_some();

    if let Some(existing) = daemon_pid() {
        if existing != std::process::id() {
            bail!(
                "aoe serve daemon already running (PID {}).\n\n  \
                 Status:  aoe serve --status\n  \
                 Open UI: aoe url\n  \
                 Stop:    aoe serve --stop",
                existing
            );
        }
    }

    let is_localhost = host_is_localhost(&args.host);

    let auth_mode = resolve_auth_mode(args.auth, args.no_auth);

    validate_auth_combination(
        auth_mode,
        args.passphrase.is_some(),
        is_localhost,
        args.behind_proxy,
        args.remote,
        &args.host,
    )?;

    validate_behind_proxy_allowlist(args.behind_proxy, args.remote, &args.allowed_host)?;
    validate_allowed_hosts(&args.allowed_host)?;
    validate_allowed_origins(&args.allowed_origin)?;

    if args.behind_proxy && args.remote {
        let msg = "--behind-proxy is ignored when --remote is set; \
             --remote already enables the equivalent cookie-Secure and \
             trusted-XFF behavior and manages its own ingress.";
        eprintln!("Note: {msg}");
        tracing::warn!(target: "serve", "{msg}");
    }

    if args.tunnel_name.is_some() && args.tunnel_url.is_none() {
        bail!(
            "Named tunnels require --tunnel-url to specify the hostname.\n\
             Example: aoe serve --remote --tunnel-name my-tunnel --tunnel-url aoe.example.com\n\
             \n\
             Setup steps:\n\
             1. cloudflared tunnel create my-tunnel\n\
             2. Add a CNAME record: aoe.example.com -> <tunnel-id>.cfargotunnel.com\n\
             3. aoe serve --remote --tunnel-name my-tunnel --tunnel-url aoe.example.com"
        );
    }

    let host = if args.remote {
        let tailscale_ok =
            tokio::task::spawn_blocking(crate::server::tunnel::tailscale_available_sync)
                .await
                .unwrap_or(false);
        if cloudflared_required(args.no_tailscale, args.tunnel_name.is_some(), tailscale_ok) {
            tokio::task::spawn_blocking(crate::server::tunnel::check_cloudflared)
                .await
                .map_err(|e| anyhow::anyhow!(e))??;
        }
        "127.0.0.1".to_string()
    } else {
        args.host.clone()
    };

    if !is_localhost && !args.remote {
        eprintln!("==========================================================");
        eprintln!("  SECURITY WARNING: Binding to {}", args.host);
        eprintln!("==========================================================");
        eprintln!();
        eprintln!("  This exposes terminal access to your network.");
        eprintln!("  Anyone with the auth token can execute commands");
        eprintln!("  as your user on this machine.");
        eprintln!();
        eprintln!("  Traffic is NOT encrypted (HTTP, not HTTPS).");
        eprintln!("  Use a VPN (Tailscale, WireGuard) or SSH tunnel");
        eprintln!("  for remote access. Do NOT expose this to the");
        eprintln!("  public internet without TLS termination.");
        eprintln!();
        eprintln!("  Or use: aoe serve --remote");
        eprintln!("  for automatic HTTPS via Tailscale Funnel");
        eprintln!("  (preferred) or Cloudflare Tunnel.");
        eprintln!();
        if args.read_only {
            eprintln!("  Read-only mode is ON: terminal input is disabled.");
            eprintln!();
        }
        if crate::server::is_wildcard_bind(&args.host) && args.allowed_host.is_empty() {
            let msg = "Wildcard bind: the LAN/VPN IP URLs above work as-is. \
                       To reach this server by a HOSTNAME or mDNS name \
                       (e.g. my-box.local), re-run with --allowed-host <name> \
                       (repeatable).";
            eprintln!("  {msg}");
            eprintln!();
            tracing::info!(target: "serve", "{msg}");
        }
        if std::env::var_os("AOE_CITYHALL_MODE").is_some() {
            eprintln!("  CityHall client mode is ON: dashboard is locked to a");
            eprintln!("  composer + structured-view end-user client. Requires an");
            eprintln!("  ACP-capable default agent; session creation is rejected");
            eprintln!("  otherwise.");
            eprintln!();
        }
        eprintln!("==========================================================");
        eprintln!();
    }

    if let Some(ref passphrase) = args.passphrase {
        if let Some(warning) = crate::server::login::check_passphrase_strength(passphrase) {
            eprintln!("{}", warning);
            eprintln!();
        }
    }

    if args.remote && args.passphrase.is_none() {
        bail!(
            "Refusing to start in remote mode without a passphrase.\n\
             --remote exposes terminal access to the internet.\n\
             Add --passphrase <VALUE> or set AOE_SERVE_PASSPHRASE."
        );
    }

    if args.daemon {
        return start_daemon(profile, &args);
    }

    apply_cityhall_bundle().await?;

    tracing::info!(
        target: "serve.daemon",
        profile = %profile,
        host = %host,
        port = args.resolved_port(),
        mode = if args.remote { "remote" } else { "local" },
        auth = ?auth_mode,
        "starting foreground serve",
    );

    if let Ok(path) = pid_file_path() {
        let _ = tokio::fs::write(&path, std::process::id().to_string()).await;
        tracing::debug!(target: "serve.lifecycle", path = %path.display(), pid = std::process::id(), "wrote pid file");
    }

    let result = crate::server::start_server(crate::server::ServerConfig {
        profile,
        host: &host,
        port: args.resolved_port(),
        auth_mode,
        read_only: args.read_only,
        remote: args.remote,
        tunnel_name: args.tunnel_name.as_deref(),
        tunnel_url: args.tunnel_url.as_deref(),
        no_tailscale: args.no_tailscale,
        is_daemon: false,
        passphrase: args.passphrase.as_deref(),
        behind_proxy: args.behind_proxy,
        open_browser: args.open,
        extra_allowed_hosts: args.allowed_host.clone(),
        extra_allowed_origins: args.allowed_origin.clone(),
    })
    .await;

    if let Ok(path) = pid_file_path() {
        let is_ours = tokio::fs::read_to_string(&path)
            .await
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .is_some_and(|pid| pid == std::process::id());
        if is_ours {
            let _ = tokio::fs::remove_file(&path).await;
            clear_serve_state().await;
        }
    }

    result
}

pub fn stdio_redirect_path() -> Result<PathBuf> {
    let dir = crate::session::get_app_dir()?;
    let log_cfg = crate::session::load_config()
        .ok()
        .flatten()
        .map(|c| c.logging)
        .unwrap_or_default();
    Ok(crate::logging::resolve_log_path(&log_cfg, &dir))
}

const BUNDLE_URL_ENV: &str = "AOE_CITYHALL_BUNDLE_URL";
const BUNDLE_TOKEN_ENV: &str = "AOE_CITYHALL_BUNDLE_TOKEN";
const BUNDLE_FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

async fn apply_cityhall_bundle() -> Result<()> {
    use crate::session::cityhall_bundle::{self, CityHallBundle};

    let Some(url) = std::env::var(BUNDLE_URL_ENV)
        .ok()
        .map(|u| u.trim().to_string())
        .filter(|u| !u.is_empty())
    else {
        return Ok(());
    };
    let token = std::env::var(BUNDLE_TOKEN_ENV).unwrap_or_default();
    let cache = cityhall_bundle::cache_path()?;

    let raw = match fetch_cityhall_bundle(&url, &token).await {
        Ok(raw) => raw,
        Err(e) if cache.exists() => {
            tracing::warn!(
                target: "serve.cityhall",
                error = %e,
                "could not refresh the CityHall config bundle; serving the cached configuration"
            );
            return Ok(());
        }
        Err(e) => {
            return Err(e.context(format!(
                "fetching the CityHall config bundle from {url}. \
                 The workspace has no cached configuration, so it cannot start."
            )))
        }
    };

    let report = cityhall_bundle::apply(&CityHallBundle::from_toml(&raw)?)?;
    tracing::info!(
        target: "serve.cityhall",
        settings = report.settings_applied,
        cloned = ?report.cloned,
        registered = ?report.registered,
        preserved = ?report.preserved,
        "applied the CityHall config bundle"
    );
    for failure in &report.failures {
        tracing::warn!(target: "serve.cityhall", "{failure}");
    }

    if let Err(e) = std::fs::write(&cache, &raw) {
        tracing::warn!(target: "serve.cityhall", error = %e, "could not cache the bundle");
    }
    Ok(())
}

async fn fetch_cityhall_bundle(url: &str, token: &str) -> Result<String> {
    let mut request = reqwest::Client::builder()
        .timeout(BUNDLE_FETCH_TIMEOUT)
        .build()?
        .get(url);
    if !token.is_empty() {
        request = request.bearer_auth(token);
    }
    let response = request.send().await?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        let body = body.trim();
        if body.is_empty() {
            bail!("CityHall returned HTTP {status}");
        }
        bail!(
            "CityHall returned HTTP {status}: {}",
            body.chars().take(300).collect::<String>()
        );
    }
    Ok(response.text().await?)
}

fn start_daemon(profile: &str, args: &ServeArgs) -> Result<()> {
    use std::process::{Command, Stdio};

    let exe = std::env::current_exe()?;
    let instance_id = uuid::Uuid::new_v4().to_string();
    let mut cmd = Command::new(exe);
    cmd.env(SERVE_INSTANCE_ENV, &instance_id);
    cmd.args([
        "serve",
        "--daemon-child",
        "--port",
        &args.resolved_port().to_string(),
        "--host",
        &args.host,
    ]);

    if args.no_auth {
        cmd.arg("--no-auth");
    }
    if let Some(mode) = args.auth {
        cmd.args(["--auth", mode.as_cli_str()]);
    }
    if args.behind_proxy {
        cmd.arg("--behind-proxy");
    }
    if args.read_only {
        cmd.arg("--read-only");
    }
    if args.cityhall {
        cmd.arg("--cityhall");
    }
    if args.remote {
        cmd.arg("--remote");
    }
    if let Some(ref name) = args.tunnel_name {
        cmd.args(["--tunnel-name", name]);
    }
    if let Some(ref url) = args.tunnel_url {
        cmd.args(["--tunnel-url", url]);
    }
    if args.no_tailscale {
        cmd.arg("--no-tailscale");
    }
    for h in &args.allowed_host {
        cmd.args(["--allowed-host", h]);
    }
    for o in &args.allowed_origin {
        cmd.args(["--allowed-origin", o]);
    }
    if let Some(ref passphrase) = args.passphrase {
        cmd.env("AOE_SERVE_PASSPHRASE", passphrase);
    }
    if !profile.is_empty() {
        cmd.args(["--profile", profile]);
    }

    cmd.stdin(Stdio::null());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid() is async-signal-safe per POSIX, which is the
        unsafe {
            cmd.pre_exec(|| {
                nix::unistd::setsid().map_err(std::io::Error::other)?;
                Ok(())
            });
        }
    }

    let stdio_path = stdio_redirect_path().ok();
    match stdio_path.as_ref().and_then(|p| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(p)
            .ok()
    }) {
        Some(log_file) => {
            let stdout = log_file.try_clone()?;
            let stderr = log_file;
            cmd.stdout(Stdio::from(stdout)).stderr(Stdio::from(stderr));
        }
        None => {
            cmd.stdout(Stdio::null()).stderr(Stdio::null());
        }
    }

    let child = cmd.spawn()?;
    let pid = child.id();

    tracing::info!(
        target: "serve.daemon",
        pid,
        profile = %profile,
        port = args.resolved_port(),
        host = %args.host,
        remote = args.remote,
        "daemon child spawned",
    );

    if let Ok(path) = pid_file_path() {
        std::fs::write(&path, pid.to_string())?;
        tracing::debug!(target: "serve.lifecycle", path = %path.display(), pid, "wrote pid file");
    }

    let launch = ServeLaunch {
        schema: SERVE_LAUNCH_SCHEMA,
        pid,
        instance_id: Some(instance_id),
        profile: profile.to_string(),
        host: args.host.clone(),
        port: args.resolved_port(),
        auth_mode: resolve_auth_mode(args.auth, args.no_auth),
        behind_proxy: args.behind_proxy,
        read_only: args.read_only,
        cityhall: args.cityhall,
        remote: args.remote,
        tunnel_name: args.tunnel_name.clone(),
        tunnel_url: args.tunnel_url.clone(),
        no_tailscale: args.no_tailscale,
        allowed_host: args.allowed_host.clone(),
        allowed_origin: args.allowed_origin.clone(),
    };
    if let Err(e) = write_serve_launch(&launch) {
        tracing::warn!(target: "serve.lifecycle", error = %e, "failed to write serve.launch");
    }

    println!("aoe serve started as daemon (PID {})", pid);
    println!("Stop with: aoe serve --stop");
    Ok(())
}

#[tracing::instrument(target = "serve.lifecycle", skip_all)]
pub async fn restart_daemon() -> Result<()> {
    let Some(pid) = daemon_pid() else {
        bail!(
            "No running aoe serve daemon to restart.\n\
             Start one with: aoe serve --daemon"
        );
    };

    let launch = read_serve_launch().map_err(|e| {
        anyhow::anyhow!(
            "Cannot restart: no usable launch state ({e}).\n\
             This daemon was not started by `aoe serve --daemon`; foreground\n\
             or service-supervised daemons must be restarted by their manager."
        )
    })?;

    if launch.pid != pid {
        bail!(
            "serve.launch records PID {} but the running daemon is PID {}; \
             refusing to restart stale state.",
            launch.pid,
            pid
        );
    }

    let passphrase = recall_serve_passphrase();
    if launch_needs_passphrase(&launch) && passphrase.is_none() {
        bail!(
            "Cannot restart: this daemon uses {} auth but no passphrase is \
             recoverable (set AOE_SERVE_PASSPHRASE).\n\
             Leaving the running daemon untouched.",
            if launch.remote {
                "remote"
            } else {
                "passphrase"
            }
        );
    }

    validate_auth_combination(
        launch.auth_mode,
        passphrase.is_some(),
        host_is_localhost(&launch.host),
        launch.behind_proxy,
        launch.remote,
        &launch.host,
    )?;
    validate_behind_proxy_allowlist(launch.behind_proxy, launch.remote, &launch.allowed_host)?;
    validate_allowed_hosts(&launch.allowed_host)?;
    validate_allowed_origins(&launch.allowed_origin)?;

    let args = launch.to_serve_args(passphrase);

    println!("Restarting aoe serve daemon (PID {pid})…");
    stop_daemon().await?;
    start_daemon(&launch.profile, &args)
}

#[tracing::instrument(target = "serve.shutdown", skip_all)]
pub(crate) async fn stop_daemon() -> Result<()> {
    let path = pid_file_path()?;

    if !path.exists() {
        tracing::warn!(target: "serve.shutdown", path = %path.display(), "no pid file; daemon not running");
        bail!(
            "No running daemon found (no PID file at {})",
            path.display()
        );
    }

    let pid_str = tokio::fs::read_to_string(&path).await?;
    let pid = parse_positive_pid(&pid_str)
        .ok_or_else(|| anyhow::anyhow!("Invalid PID in {}: {}", path.display(), pid_str.trim()))?;
    tracing::info!(target: "serve.shutdown", pid, "sending SIGTERM to daemon");

    match inspect_daemon_process(pid) {
        DaemonProcessIdentity::Verified => {
            if serve_launch_contradicts(pid as u32) {
                bail!(
                    "PID {} is an aoe serve process, but not the daemon recorded in \
                     serve.launch (recycled PID); preserving serve state.",
                    pid
                );
            }
        }
        DaemonProcessIdentity::Foreign => {
            remove_stale_serve_state(&path);
            bail!(
                "PID {} belongs to a different process (stale PID file). Cleaned up.",
                pid
            );
        }
        DaemonProcessIdentity::Indeterminate => {
            bail!(
                "Could not verify whether PID {} is an aoe serve daemon; \
                 preserving serve state. Retry once process inspection works.",
                pid
            );
        }
    }

    match nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid),
        nix::sys::signal::Signal::SIGTERM,
    ) {
        Ok(()) => {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                match nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None) {
                    Err(nix::errno::Errno::ESRCH) => break,
                    _ if std::time::Instant::now() >= deadline => {
                        let _ = nix::sys::signal::kill(
                            nix::unistd::Pid::from_raw(pid),
                            nix::sys::signal::Signal::SIGKILL,
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        break;
                    }
                    _ => {}
                }
            }
            let _ = tokio::fs::remove_file(&path).await;
            clear_serve_state().await;
            println!("Stopped aoe serve daemon (PID {})", pid);
        }
        Err(nix::errno::Errno::ESRCH) => {
            tokio::fs::remove_file(&path).await?;
            clear_serve_state().await;
            println!("Daemon was not running (stale PID file cleaned up)");
        }
        Err(e) => bail!("Failed to stop daemon (PID {}): {}", pid, e),
    }

    Ok(())
}

async fn print_status() -> Result<()> {
    if let Some(endpoint) = crate::acp::client::discovery::discover_env() {
        let client = crate::acp::client::HttpClient::new(endpoint.clone())
            .map_err(|e| anyhow::anyhow!("http client init failed: {e}"))?;
        match client.health_check().await {
            Ok(()) => {
                println!("Daemon: reachable (remote via AOE_DAEMON_URL)");
                println!("URL:    {}", endpoint.base_url);
                println!(
                    "Token:  {}",
                    if endpoint.has_token() { "set" } else { "unset" }
                );
                Ok(())
            }
            Err(e) => bail!(
                "AOE_DAEMON_URL is set but the daemon at {} is unreachable ({e}); \
                 check the address or unset to use a local daemon",
                endpoint.base_url
            ),
        }
    } else {
        print_local_status()
    }
}

fn print_local_status() -> Result<()> {
    let Some(pid) = daemon_pid() else {
        bail!("Daemon: not running\nStart one with: aoe serve --daemon");
    };

    let mode = read_serve_mode_label().unwrap_or("unknown");
    let urls = read_serve_urls();
    let log_path = stdio_redirect_path().ok();

    println!("Daemon: running (PID {})", pid);
    println!("Mode:   {}", mode);
    if let Some(primary) = urls.first() {
        println!("URL:    {}", primary.url);
        for u in urls.iter().skip(1) {
            let label = u.label.as_deref().unwrap_or("alt");
            println!("        {} {}", label, u.url);
        }
    } else {
        println!("URL:    (serve.url missing)");
    }
    if let Some(p) = log_path {
        println!("Log:    {}", p.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloudflared_required_unless_tailscale_serves_the_default_flags() {
        let cases = [
            ("default flags with tailscale", false, false, true, false),
            ("--no-tailscale", true, false, true, true),
            ("named tunnel pinned", false, true, true, true),
            ("tailscale unavailable", false, false, false, true),
        ];
        for (name, no_tailscale, named_tunnel, tailscale_available, expected) in cases {
            assert_eq!(
                cloudflared_required(no_tailscale, named_tunnel, tailscale_available),
                expected,
                "{name}"
            );
        }
    }

    #[test]
    fn daemon_probe_only_marks_missing_process_as_stale() {
        let cases = [
            (Ok(()), DaemonProbeDisposition::VerifyIdentity),
            (Err(nix::errno::Errno::ESRCH), DaemonProbeDisposition::Stale),
            (
                Err(nix::errno::Errno::EPERM),
                DaemonProbeDisposition::Indeterminate,
            ),
            (
                Err(nix::errno::Errno::EIO),
                DaemonProbeDisposition::Indeterminate,
            ),
        ];
        for (result, expected) in cases {
            assert_eq!(classify_daemon_probe(result), expected);
        }
    }

    #[test]
    fn daemon_command_requires_exact_executable_and_serve_subcommand() {
        let cases: &[(&[u8], bool)] = &[
            (b"/usr/local/bin/aoe\0serve\0--daemon\0", true),
            (b"agent-of-empires serve --daemon", true),
            (b"aoe --profile work serve --daemon", true),
            (b"aoe --profile=work serve --daemon", true),
            (b"aoe update", false),
            (b"aoe --profile serve update", false),
            (b"aoe --some-option serve update", false),
            (b"/tmp/aoe-helper serve", false),
            (b"runner --label aoe serve", false),
        ];
        for (command, expected) in cases {
            assert_eq!(command_is_aoe_serve(command), *expected, "{command:?}");
        }
    }

    #[test]
    fn daemon_pid_must_be_positive() {
        let cases = [
            ("42", Some(42)),
            (" 7\n", Some(7)),
            ("0", None),
            ("-1", None),
            ("x", None),
        ];
        for (raw, expected) in cases {
            assert_eq!(parse_positive_pid(raw), expected, "{raw:?}");
        }
    }

    #[test]
    fn daemon_instance_requires_an_exact_environment_entry() {
        let cases: &[(&[u8], &str, bool)] = &[
            (b"HOME=/tmp\0AOE_SERVE_INSTANCE_ID=abc\0", "abc", true),
            (b"aoe serve AOE_SERVE_INSTANCE_ID=abc", "abc", true),
            (b"AOE_SERVE_INSTANCE_ID=other\0", "abc", false),
            (b"PREFIX_AOE_SERVE_INSTANCE_ID=abc\0", "abc", false),
            (b"AOE_SERVE_INSTANCE_ID=abc-suffix", "abc", false),
        ];
        for (environment, instance_id, expected) in cases {
            assert_eq!(
                environment_has_daemon_instance(environment, instance_id),
                *expected,
                "{environment:?}"
            );
        }
    }

    #[test]
    fn host_is_localhost_accepts_only_loopback_forms() {
        let cases = [
            ("localhost", true),
            ("127.0.0.1", true),
            ("::1", true),
            ("0.0.0.0", false),
            ("192.168.1.1", false),
            ("aoe.example.com", false),
        ];
        for (host, expected) in cases {
            assert_eq!(host_is_localhost(host), expected, "{host}");
        }
    }

    #[test]
    fn resolve_auth_mode_defaults_to_token_and_honors_explicit_choices() {
        let cases = [
            (None, false, AuthMode::Token),
            (None, true, AuthMode::None),
            (Some(AuthMode::Passphrase), false, AuthMode::Passphrase),
            (Some(AuthMode::None), false, AuthMode::None),
        ];
        for (auth, no_auth, expected) in cases {
            assert_eq!(resolve_auth_mode(auth, no_auth), expected, "{auth:?}");
        }
    }

    #[test]
    fn validate_auth_combination_gates_reduced_auth_modes() {
        let err = |mode, has_passphrase, is_localhost, behind_proxy, remote, host: &str| {
            validate_auth_combination(
                mode,
                has_passphrase,
                is_localhost,
                behind_proxy,
                remote,
                host,
            )
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default()
        };
        const LOCAL: &str = "127.0.0.1";
        const WIDE: &str = "0.0.0.0";

        assert_eq!(err(AuthMode::Token, false, true, false, false, LOCAL), "");
        assert_eq!(err(AuthMode::Token, true, true, false, true, LOCAL), "");
        assert_eq!(
            err(AuthMode::Passphrase, true, true, false, false, LOCAL),
            ""
        );
        assert_eq!(err(AuthMode::None, false, true, false, false, LOCAL), "");
        assert_eq!(
            err(AuthMode::Passphrase, true, false, true, false, WIDE),
            ""
        );

        assert!(err(AuthMode::Passphrase, false, true, false, false, LOCAL)
            .contains("--auth=passphrase requires"));
        assert!(err(AuthMode::None, true, true, false, false, LOCAL)
            .contains("--auth=none does not honor --passphrase"));
        assert!(
            err(AuthMode::Passphrase, true, false, false, false, WIDE).contains("--behind-proxy")
        );
        assert!(err(AuthMode::None, false, false, false, false, WIDE).contains("--behind-proxy"));
        assert!(
            err(AuthMode::Passphrase, true, true, false, true, LOCAL).contains("in remote mode")
        );
        assert!(err(AuthMode::None, false, true, false, true, LOCAL).contains("in remote mode"));
    }

    #[test]
    fn auth_mode_cli_str_matches_clap_and_serde() {
        for variant in <AuthMode as ValueEnum>::value_variants() {
            let cli_str = variant.as_cli_str();
            let parsed = AuthMode::from_str(cli_str, true).unwrap_or_else(|_| {
                panic!("clap rejects as_cli_str() output {:?}", cli_str);
            });
            assert_eq!(parsed, *variant);
            let pv = variant
                .to_possible_value()
                .expect("non-skipped variant has a PossibleValue");
            assert_eq!(pv.get_name(), cli_str);

            let json = serde_json::to_string(variant).expect("serialize AuthMode");
            assert_eq!(json, format!("\"{cli_str}\""));
            let back: AuthMode = serde_json::from_str(&json).expect("deserialize AuthMode");
            assert_eq!(back, *variant);
        }
    }

    fn sample_launch() -> ServeLaunch {
        ServeLaunch {
            schema: SERVE_LAUNCH_SCHEMA,
            pid: 4242,
            instance_id: Some("instance-1".to_string()),
            profile: "work".to_string(),
            host: "0.0.0.0".to_string(),
            port: 9090,
            auth_mode: AuthMode::Passphrase,
            behind_proxy: true,
            read_only: true,
            cityhall: true,
            remote: false,
            tunnel_name: Some("named".to_string()),
            tunnel_url: Some("aoe.example.com".to_string()),
            no_tailscale: true,
            allowed_host: vec!["aoe.example.com".to_string()],
            allowed_origin: vec!["https://aoe.example.com:8443".to_string()],
        }
    }

    #[test]
    fn parser_knows_every_top_level_global() {
        use clap::CommandFactory;
        let mut globals: Vec<(String, Option<char>)> = crate::cli::definition::Cli::command()
            .get_arguments()
            .filter(|arg| arg.is_global_set())
            .map(|arg| {
                (
                    arg.get_long().unwrap_or_default().to_string(),
                    arg.get_short(),
                )
            })
            .collect();
        globals.sort();
        assert_eq!(
            globals,
            vec![
                ("daemon-url".to_string(), None),
                ("profile".to_string(), Some('p')),
            ],
            "a top-level global changed; teach command_is_aoe_serve about it before \
             updating this list, or an `aoe serve` command line carrying the new \
             option will be classified as a foreign process",
        );
    }

    #[test]
    fn space_joined_ps_output_is_unverifiable_not_foreign() {
        let cases = [
            (&b"/Users/me/My Apps/aoe serve --daemon-child"[..], true),
            (
                &b"/usr/local/bin/aoe -p my profile serve --daemon-child"[..],
                true,
            ),
            (
                &b"/usr/local/bin/aoe --profile work --host 0.0.0.0 --port"[..],
                true,
            ),
            (&b"/opt/homebrew/bin/agent-of-empires serve"[..], true),
            (&b"/usr/bin/vim src/aoe.rs"[..], false),
            (&b"/usr/bin/python3 manage.py runserver"[..], false),
        ];
        for (command, expected) in cases {
            assert_eq!(
                command_mentions_aoe_executable(command),
                expected,
                "{}",
                String::from_utf8_lossy(command)
            );
        }
    }

    #[test]
    fn launch_contradiction_only_flags_recycled_pids() {
        let cases = [
            (4242, Some("instance-1"), false, true, false),
            (4242, Some("instance-1"), true, false, true),
            (99, Some("instance-1"), false, false, false),
            (4242, None, false, false, true),
        ];
        for (launch_pid, instance_id, live, contradicts, authorizes) in cases {
            let launch = ServeLaunch {
                pid: launch_pid,
                instance_id: instance_id.map(str::to_string),
                ..sample_launch()
            };
            let label = format!("pid {launch_pid} instance {instance_id:?} live {live}");
            assert_eq!(
                launch_contradicts(&launch, 4242, live),
                contradicts,
                "contradicts: {label}"
            );
            assert_eq!(
                launch_authorizes(&launch, 4242, live),
                authorizes,
                "authorizes: {label}"
            );
        }
    }

    #[test]
    fn to_serve_args_replays_launch_config() {
        let launch = sample_launch();
        let args = launch.to_serve_args(Some("hunter2".to_string()));
        assert_eq!(args.port, Some(9090));
        assert_eq!(args.host, "0.0.0.0");
        assert_eq!(args.auth, Some(AuthMode::Passphrase));
        assert!(!args.no_auth);
        assert!(args.behind_proxy);
        assert!(args.read_only);
        assert!(args.cityhall);
        assert_eq!(args.tunnel_name.as_deref(), Some("named"));
        assert_eq!(args.tunnel_url.as_deref(), Some("aoe.example.com"));
        assert!(args.no_tailscale);
        assert!(args.daemon);
        assert!(!args.daemon_child);
        assert!(!args.restart);
        assert!(!args.stop);
        assert_eq!(args.passphrase.as_deref(), Some("hunter2"));
        assert_eq!(args.allowed_host, vec!["aoe.example.com".to_string()]);
        assert_eq!(
            args.allowed_origin,
            vec!["https://aoe.example.com:8443".to_string()]
        );
    }

    #[test]
    fn behind_proxy_needs_an_allowed_host_unless_remote() {
        let err = validate_behind_proxy_allowlist(true, false, &[])
            .expect_err("behind-proxy with no allowed host must be rejected");
        assert!(err.to_string().contains("--allowed-host"));
        validate_behind_proxy_allowlist(true, false, &["aoe.example.com".to_string()])
            .expect("behind-proxy with an allowed host starts");
        validate_behind_proxy_allowlist(true, true, &[])
            .expect("remote auto-injects the tunnel host, so no flag is required");
    }

    #[test]
    fn allowed_origins_need_a_scheme_a_host_and_nothing_else() {
        for origin in [
            "aoe.example.com:8443",
            "",
            "https://",
            "https:///",
            "https://:8443",
            "https://aoe.example.com/app",
            "https://aoe.example.com?x",
            "https://user@aoe.example.com",
            "http://0.0.0.0:8080",
            "https://[::]",
            "http://169.254.169.254",
            "https://[fe80::1]:8443",
            "http://224.0.0.1",
        ] {
            assert!(
                validate_allowed_origins(&[origin.to_string()]).is_err(),
                "{origin:?} must be rejected"
            );
        }
        validate_allowed_origins(&[
            "https://aoe.example.com:8443".to_string(),
            "http://localhost:3000".to_string(),
            "HTTPS://aoe.example.com".to_string(),
            "https://aoe.example.com/".to_string(),
            "https://[::1]".to_string(),
            "http://127.0.0.1:3000".to_string(),
            "https://192.168.1.5:8443".to_string(),
        ])
        .expect("scheme://host[:port] origins, IPv6 and routable literals included, are accepted");
    }

    #[test]
    fn allowed_hosts_take_a_bare_authority_and_reject_untrusted_literals() {
        validate_allowed_hosts(&[
            "aoe.example.com".to_string(),
            "aoe.example.com:8443".to_string(),
            "192.168.1.5".to_string(),
            "2001:db8::1".to_string(),
            "[::1]:8080".to_string(),
            "127.0.0.1".to_string(),
            "::1".to_string(),
        ])
        .expect("a bare host or host:port (incl. IPv6 and loopback) is a valid --allowed-host");

        for host in [
            "0.0.0.0",
            "0.0.0.0:8080",
            "::",
            "[::]:8080",
            "169.254.169.254",
            "fe80::1",
            "[fe80::1]:8080",
            "::ffff:169.254.169.254",
            "224.0.0.1",
            "ff02::1",
            "https://aoe.example.com",
            "aoe.example.com/app",
            "aoe.example.com?x",
            "user@aoe.example.com",
            ":8080",
            ":",
            "   ",
        ] {
            assert!(
                validate_allowed_hosts(&[host.to_string()]).is_err(),
                "{host:?} must be rejected"
            );
        }
    }

    #[test]
    fn launch_needs_passphrase_for_remote_and_passphrase_auth() {
        let mut launch = sample_launch();
        launch.auth_mode = AuthMode::Passphrase;
        launch.remote = false;
        assert!(launch_needs_passphrase(&launch));

        launch.auth_mode = AuthMode::Token;
        launch.remote = true;
        assert!(launch_needs_passphrase(&launch));

        launch.auth_mode = AuthMode::Token;
        launch.remote = false;
        assert!(!launch_needs_passphrase(&launch));
    }

    mod profile_guard {
        use super::super::run;
        use crate::cli::{Cli, Commands};
        use clap::Parser;
        use serial_test::serial;
        use std::path::PathBuf;

        fn dispatch_argv(argv: &[&str]) -> (String, super::super::ServeArgs) {
            let cli = Cli::try_parse_from(argv).expect("argv parses");
            let profile = cli.profile.unwrap_or_default();
            match cli.command {
                Some(Commands::Serve(args)) => (profile, args),
                _ => panic!("expected a serve invocation"),
            }
        }

        fn armed_profiles_dir() -> (crate::session::test_support::AppDirGuard, PathBuf) {
            let guard = crate::session::test_support::isolate_app_dir();
            let profiles = crate::session::get_app_dir().unwrap().join("profiles");
            std::fs::create_dir_all(profiles.join("real")).unwrap();
            (guard, profiles)
        }

        #[tokio::test]
        #[serial]
        async fn lifecycle_verbs_never_consult_the_profile() {
            let (_guard, profiles) = armed_profiles_dir();
            for verb in ["--stop", "--status", "--restart"] {
                let (profile, args) = dispatch_argv(&["aoe", "serve", verb, "-p", "ghost-profile"]);
                if let Err(e) = run(&profile, args).await {
                    let msg = e.to_string();
                    assert!(
                        !msg.contains("does not exist") && !msg.contains("aoe profile create"),
                        "`serve {verb}` must not check the profile, got: {msg}"
                    );
                }
                assert!(
                    !profiles.join("ghost-profile").exists(),
                    "`serve {verb}` must not mint profiles/ghost-profile"
                );
            }
        }

        #[tokio::test]
        #[serial]
        async fn fresh_start_refuses_unknown_profile_before_any_side_effect() {
            let (_guard, profiles) = armed_profiles_dir();
            let (profile, args) =
                dispatch_argv(&["aoe", "serve", "--behind-proxy", "-p", "ghost-profile"]);
            let msg = run(&profile, args)
                .await
                .expect_err("unknown profile must refuse a fresh start")
                .to_string();
            assert!(
                msg.contains("Profile 'ghost-profile' does not exist")
                    && msg.contains("aoe profile create ghost-profile"),
                "expected the unknown-profile error first, got: {msg}"
            );
            assert!(!profiles.join("ghost-profile").exists());
            assert!(
                !super::super::pid_file_path().unwrap().exists(),
                "a refused start must leave no PID file behind"
            );
        }

        #[tokio::test]
        #[serial]
        async fn fresh_start_lets_a_known_profile_through_to_argument_validation() {
            let (_guard, _profiles) = armed_profiles_dir();
            let (profile, args) = dispatch_argv(&["aoe", "serve", "--behind-proxy", "-p", "real"]);
            let msg = run(&profile, args)
                .await
                .expect_err("--behind-proxy without --allowed-host is refused")
                .to_string();
            assert!(
                msg.contains("--behind-proxy requires --allowed-host"),
                "a known profile must reach the next validation, got: {msg}"
            );
        }
    }
}
