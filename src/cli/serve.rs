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
    /// CLI string form, matching what `--auth=<MODE>` accepts. The
    /// match arms are kept in lockstep with clap's `value(rename_all =
    /// "lowercase")` derive by the `auth_mode_cli_str_matches_clap`
    /// unit test, which round-trips each string through `ValueEnum`.
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
    /// Serve the private local API without opening TCP or a dashboard.
    #[arg(long, conflicts_with_all = ["remote", "behind_proxy", "port", "host", "auth", "no_auth", "open", "passphrase", "allowed_host", "allowed_origin"])]
    pub core_only: bool,

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
    /// the `X-Forwarded-For` header from loopback peers. Does NOT auto-spawn a tunnel (unlike --remote).
    /// Required when --auth=passphrase or --auth=none is combined with
    /// a non-loopback bind.
    #[arg(long)]
    pub behind_proxy: bool,

    /// Accept plain HTTP from non-loopback peers. By default the server answers
    /// those with 426 before authentication: bearer tokens and passphrases would
    /// cross the network in the clear. A TLS proxy or tunnel in front of a
    /// loopback listener (`--behind-proxy`, `--remote`) needs no opt-out. This
    /// applies to this bind only, is never inferred from the address, and is
    /// logged at every start.
    #[arg(long)]
    pub dangerously_allow_plaintext_remote: bool,

    /// Extra `Host` header value to accept (repeatable). The DNS-rebinding
    /// gate trusts loopback, any routable IP-literal `Host` (LAN/tailnet IPs
    /// can't be rebound), and a non-wildcard `--host` by default. A browser
    /// `Origin` is accepted without `--allowed-origin` only when it is
    /// allowlisted or has the exact same authority as `Host`; add a HOSTNAME
    /// or mDNS name here when serving behind a reverse proxy, a custom tunnel,
    /// or by name when binding `0.0.0.0` (access by IP needs no flag).
    /// Auto-injected tunnel hosts (`--remote`) need no flag.
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

    /// Print the local daemon's PID, mode, URLs, and log path. With
    /// `AOE_DAEMON_URL` set, report the remote daemon's reachability and
    /// token state instead. Exits non-zero when no daemon is running.
    /// Useful for shell scripts that want to know whether a daemon is up
    /// without parsing `ps`.
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
            "no_auth", "auth", "behind_proxy", "dangerously_allow_plaintext_remote",
            "read_only", "cityhall", "passphrase", "port",
            "tunnel_name", "no_tailscale", "tunnel_url", "open",
            "allowed_host", "allowed_origin",
        ],
    )]
    pub status: bool,

    /// Require a passphrase for login (second-factor auth).
    /// Can also be set via AOE_SERVE_PASSPHRASE environment variable.
    #[arg(long)]
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

    /// Restart the running managed daemon with its recorded policy.
    /// Missing credentials are rejected before stopping it.
    #[arg(
        long,
        conflicts_with_all = [
            "stop", "status", "daemon", "daemon_child", "core_only", "remote", "rollback",
            "no_auth", "auth", "behind_proxy", "dangerously_allow_plaintext_remote",
            "read_only", "cityhall", "passphrase", "port", "host",
            "tunnel_name", "no_tailscale", "tunnel_url", "open",
            "allowed_host", "allowed_origin",
        ],
    )]
    pub restart: bool,

    /// Restore the retained managed launch, including a core replaced by the dashboard.
    #[arg(long, conflicts_with_all = [
        "stop", "status", "restart", "daemon", "daemon_child", "core_only",
        "remote", "no_auth", "auth", "behind_proxy", "read_only", "cityhall",
        "passphrase", "port", "host", "tunnel_name", "no_tailscale",
        "tunnel_url", "open", "allowed_host", "allowed_origin",
    ])]
    pub rollback: bool,
}

impl ServeArgs {
    /// Resolve the port: explicit `--port` wins; otherwise 8081 in debug
    /// builds, 8080 in release. The `-dev` suffix on the app dir keeps
    /// state isolated, but two daemons cannot share a port, so the default
    /// shifts as well.
    pub fn resolved_port(&self) -> u16 {
        self.port
            .unwrap_or(if cfg!(debug_assertions) { 8081 } else { 8080 })
    }
}

/// Pure check used by both the CLI validator and its unit tests.
fn host_is_localhost(host: &str) -> bool {
    host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Resolve the effective `AuthMode` from the two CLI surfaces
/// (`--auth=<mode>` and the legacy `--no-auth` alias). Clap's
/// `conflicts_with` already rejects passing both, so the
/// `(Some, true)` arm is unreachable in practice.
fn resolve_auth_mode(auth: Option<AuthMode>, no_auth: bool) -> AuthMode {
    match (auth, no_auth) {
        (Some(mode), false) => mode,
        (None, true) => AuthMode::None,
        (None, false) => AuthMode::Token,
        (Some(_), true) => unreachable!("clap conflicts_with prevents this"),
    }
}

/// Reject mode + flag combinations that the daemon refuses to start
/// with. Pure for unit testing; produces the same `anyhow::Error`
/// shape as the inline guards used to.
fn validate_auth_combination(
    auth_mode: AuthMode,
    has_passphrase: bool,
    is_localhost: bool,
    behind_proxy: bool,
    remote: bool,
    host: &str,
) -> Result<()> {
    // --auth=passphrase needs a passphrase: passphrase is the sole
    // human gate, an empty wall means no auth at all.
    if matches!(auth_mode, AuthMode::Passphrase) && !has_passphrase {
        bail!(
            "--auth=passphrase requires --passphrase <VALUE> or AOE_SERVE_PASSPHRASE.\n\
             Without a passphrase there is no gate. Use --auth=none if that is intended."
        );
    }

    // --auth=none silently discarding a provided passphrase is the
    // legacy misleading behavior of `--no-auth --passphrase`; reject
    // explicitly so the user picks the mode they actually want.
    if matches!(auth_mode, AuthMode::None) && has_passphrase {
        bail!("--auth=none does not honor --passphrase; use --auth=passphrase instead.");
    }

    // Reduced-auth modes on a non-loopback bind require an upstream
    // proxy that terminates TLS.
    if matches!(auth_mode, AuthMode::None | AuthMode::Passphrase) && !is_localhost && !behind_proxy
    {
        bail!(
            "Refusing to start with --auth={} on {}.\n\
             Reduced-auth modes on a non-loopback bind require --behind-proxy,\n\
             which signals that an upstream reverse proxy terminates TLS and\n\
             forwards the client IP via X-Forwarded-For.",
            auth_mode.as_cli_str(),
            host
        );
    }

    // Block reduced-auth with --remote: --remote auto-spawns a public
    // ingress and mandates token + passphrase. Collapsing the token
    // away (or dropping auth entirely) on a publicly-reachable tunnel
    // is never the intent.
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

/// A daemon behind an external reverse proxy (`--behind-proxy`, no `--remote`)
/// answers to the operator's public hostname, which aoe cannot derive (there
/// is no tunnel handle to read it from). Without at least one `--allowed-host`
/// the DNS-rebinding gate would 403 every proxied request, so refuse to start
/// with an explicit message instead of failing silently at runtime (#2735).
/// `--remote` is exempt: it auto-injects the tunnel host.
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

/// Characters a bare `Host` value or an `Origin` authority can never contain:
/// their presence means a path, query, fragment, or userinfo crept in, so the
/// value can never equal a browser-sent `Host`/`Origin`. Shared by both
/// allowlist validators so the two lists cannot drift (#2735).
const FORBIDDEN_AUTHORITY_CHARS: [char; 4] = ['/', '?', '#', '@'];

/// A browser `Origin` is always `scheme://host[:port]` with no path, so a
/// schemeless (`aoe.example.com:8443`), hostless (`https://`, `https://:8443`),
/// path/query/userinfo-bearing (`https://x/app`, `https://x?y`, `https://u@x`)
/// `--allowed-origin` normalizes to a value no `Origin` header can ever equal:
/// it would silently 403 the very requests it was meant to permit. Reject it at
/// startup with the corrected form instead of failing closed at runtime (#2735).
fn validate_allowed_origins(allowed_origins: &[String]) -> Result<()> {
    for origin in allowed_origins {
        let lower = origin.trim().to_ascii_lowercase();
        let host = lower
            .strip_prefix("https://")
            .or_else(|| lower.strip_prefix("http://"));
        // A purely trailing slash is harmless (`norm_origin` strips it). Reject
        // a host that carries a path/query/userinfo or normalizes to nothing
        // (e.g. `:8443`), since none can equal a browser `Origin`.
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

/// A `--allowed-host` is a bare `Host` value (a port is harmless: `norm_host`
/// strips it symmetrically on both sides), never a scheme, path, query, or
/// userinfo. A pasted URL (`https://aoe.example.com`), a path
/// (`aoe.example.com/app`), or a port-only value (`:8080`, which `norm_host`
/// collapses to nothing yet satisfies `--behind-proxy`'s non-empty check) leaves
/// a value the gate can never match, silently 403ing the requests it was meant
/// to permit. Reject such values at startup instead of failing closed at
/// runtime (#2735).
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

/// True when `aoe serve --remote` will route through Cloudflare and therefore
/// needs `cloudflared` on PATH. That covers both an explicit named tunnel
/// (`--tunnel-name`) and the quick-tunnel fallback path that runs when
/// Tailscale isn't usable or the user passed `--no-tailscale`. Mirrors the
/// transport selection inside `start_server()` so the early guard doesn't
/// reject Tailscale-only setups (issue #813).
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

/// Held by a foreground `aoe serve` for its lifetime and by `aoe migrate` for its run,
/// so the two never overlap. The OS drops it with the process.
#[derive(Debug)]
pub struct ServeLock {
    _file: std::fs::File,
}

pub fn try_lock_serve() -> Result<Option<ServeLock>> {
    try_lock_serve_at(&crate::session::get_app_dir()?.join("serve.lock"))
}
pub(crate) fn with_inactive_serve_state<T>(
    transaction: &crate::daemon::lifecycle::Transaction,
    serve: Option<&ServeLock>,
    operation: impl FnOnce(&ServeLock) -> Result<T>,
) -> Result<T> {
    let owned;
    let serve = match serve {
        Some(serve) => serve,
        None => {
            owned = try_lock_serve()?
                .context("A daemon or migration owns serve.lock; recovery refused")?;
            &owned
        }
    };
    let _lifetime = transaction
        .try_lifetime()?
        .context("A daemon still owns its lifetime; recovery refused")?;
    let _state = lock_serve_state_for_recovery()?;
    anyhow::ensure!(
        daemon_status() == DaemonStatus::Absent,
        "Existing daemon identity is live or ambiguous; recovery refused"
    );
    operation(serve)
}

fn lock_serve_state_for_recovery() -> Result<std::fs::File> {
    use fs2::FileExt as _;
    let root = crate::session::AnchoredDir::open(&crate::session::get_app_dir()?)?;
    let (file, path) = root.open_lock("serve.state.lock".as_ref())?;
    crate::process::validate_recovery_file(&file)?;
    match file.try_lock_exclusive() {
        Ok(()) => Ok(file),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
            anyhow::bail!(
                "Serve-state publisher owns {}; recovery refused without changing authority",
                path.display()
            )
        }
        Err(error) => Err(error).with_context(|| {
            format!(
                "Acquiring recovery serve-state exclusion {}",
                path.display()
            )
        }),
    }
}

pub struct ServeStartup {
    lock: ServeLock,
    transaction: crate::daemon::lifecycle::Transaction,
}

impl ServeStartup {
    pub fn run_migrations(
        &self,
        reporter: Option<crate::migrations::progress::Reporter>,
    ) -> Result<()> {
        crate::migrations::run_migrations_under_exclusion(
            reporter,
            false,
            &self.transaction,
            &self.lock,
        )
    }
}

/// Establish lifecycle authority and serve/migrate exclusion before migrations.
pub async fn lock_for_startup(profile: &str, args: &mut ServeArgs) -> Result<Option<ServeStartup>> {
    if args.stop || args.status || args.restart || args.rollback {
        return Ok(None);
    }
    let mut transaction = if args.daemon_child {
        crate::daemon::lifecycle::Transaction::receive()?
    } else {
        crate::daemon::lifecycle::Transaction::acquire().await?
    };
    if !args.core_only {
        args.passphrase = resolve_launch_passphrase(args.passphrase.take(), None, || {
            std::env::var("AOE_SERVE_PASSPHRASE").ok()
        });
    }
    args.cityhall = args.cityhall || std::env::var_os("AOE_CITYHALL_MODE").is_some();
    match daemon_status() {
        DaemonStatus::Verified(pid) if pid != std::process::id() => {
            transaction = migrate_managed_launch(transaction, false).await?;
            let launch = managed_launch(pid)?;
            if !launch.core_only || args.core_only || args.daemon_child {
                return Err(daemon_running_error(pid));
            }
            validate_launch_args(profile, args).await?;
            let passphrase = recall_serve_passphrase(&launch)?;
            retain_rollback_launch(&transaction, &launch, passphrase.as_deref())?;
            stop_daemon_locked(&transaction).await?;
        }
        DaemonStatus::Unverified => {
            bail!("Cannot verify the existing daemon; refusing replacement")
        }
        _ => {}
    }
    let lock = try_lock_serve()?.context(
        "another aoe serve or `aoe migrate` is starting or running; retry once it finishes",
    )?;
    Ok(Some(ServeStartup { lock, transaction }))
}

fn daemon_running_error(pid: u32) -> anyhow::Error {
    anyhow::anyhow!(
        "aoe serve daemon already running (PID {pid}).\n\n  \
         Status:  aoe serve --status\n  \
         Open UI: aoe url\n  \
         Stop:    aoe serve --stop"
    )
}

fn try_lock_serve_at(path: &std::path::Path) -> Result<Option<ServeLock>> {
    use fs2::FileExt as _;
    let directory =
        crate::session::AnchoredDir::open(path.parent().context("Serve lock has no parent")?)?;
    let (file, _) = directory.open_lock(path.file_name().context("Serve lock has no name")?)?;
    crate::process::validate_recovery_file(&file)?;
    match file.try_lock_exclusive() {
        Ok(()) => Ok(Some(ServeLock { _file: file })),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
        Err(error) => Err(error.into()),
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ServeLaunch {
    pub schema: u32,
    pub pid: u32,
    /// Absent in records written before instance IDs existed (schema 1).
    #[serde(default)]
    pub instance_id: Option<String>,
    pub profile: String,
    /// Absent in records written before core-only launches existed (schema 2);
    /// such a record is always a full daemon, so `false` is the value it
    /// would have been persisted with.
    #[serde(default)]
    pub core_only: bool,
    pub host: String,
    pub port: u16,
    pub auth_mode: AuthMode,
    pub has_passphrase: bool,
    pub behind_proxy: bool,
    #[serde(default)]
    pub allow_plaintext_remote: bool,
    pub read_only: bool,
    /// CityHall client mode. Persisted so `--restart` and the `aoe update`
    /// re-exec replay it; without this the mode would silently drop because it
    /// is otherwise only carried by the `AOE_CITYHALL_MODE` env var (#7).
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
    /// Preserve unknown options when a catalogue mutation only changes the profile.
    #[serde(flatten)]
    pub additional_fields: serde_json::Map<String, serde_json::Value>,
}

pub(crate) const SERVE_LAUNCH_SCHEMA: u32 = 4;
const SERVE_INSTANCE_ENV: &str = "AOE_SERVE_INSTANCE_ID";

impl ServeLaunch {
    /// Rebuild the `ServeArgs` needed to relaunch this daemon. The
    /// effective auth mode is replayed via `--auth`; clap's `--no-auth`
    /// alias is not needed since `--auth=none` covers it. The passphrase
    /// is resolved and injected by the caller (see
    /// [`resolve_launch_passphrase`]).
    fn to_serve_args(&self, passphrase: Option<String>) -> ServeArgs {
        ServeArgs {
            core_only: self.core_only,
            port: (!self.core_only).then_some(self.port),
            host: self.host.clone(),
            auth: (!self.core_only).then_some(self.auth_mode),
            no_auth: false,
            behind_proxy: self.behind_proxy,
            dangerously_allow_plaintext_remote: self.allow_plaintext_remote,
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
            rollback: false,
            allowed_host: self.allowed_host.clone(),
            allowed_origin: self.allowed_origin.clone(),
        }
    }
}

impl ServeLaunch {
    /// Replay the recorded launch, taking only what the TUI restart controls
    /// (bind, exposure, port, dashboard enablement, passphrase) from `requested`,
    /// so recorded policy such as `--read-only` and the allowlists survives.
    fn overlay_tui_args(&self, requested: ServeArgs) -> ServeArgs {
        let mut args = self.to_serve_args(None);
        if self.core_only {
            args.auth = requested.auth;
        }
        args.core_only = requested.core_only;
        args.port = requested.port;
        args.host = requested.host;
        args.remote = requested.remote;
        if !args.remote {
            args.tunnel_name = None;
            args.tunnel_url = None;
        }
        args.no_tailscale = requested.no_tailscale;
        args.passphrase = requested.passphrase;
        args
    }
}

fn launch_file(name: &str) -> Result<crate::session::ResolvedDataFile> {
    crate::session::AnchoredDir::open(&crate::session::get_app_dir()?)?
        .bind_file(std::ffi::OsStr::new(name))
}

fn write_serve_launch(state: &ServeLaunch) -> Result<()> {
    launch_file("serve.launch")?.replace(&serde_json::to_vec_pretty(state)?)
}

fn read_serve_launch() -> Result<ServeLaunch> {
    let raw = launch_file("serve.launch")?
        .read()?
        .context("Missing daemon launch record")?;
    serde_json::from_str(&raw).context("Invalid daemon launch record")
}

fn managed_launch(pid: u32) -> Result<ServeLaunch> {
    let launch = read_serve_launch().context(
        "No usable managed launch state; foreground and service-supervised daemons must be stopped by their manager",
    )?;
    anyhow::ensure!(
        launch.schema == SERVE_LAUNCH_SCHEMA,
        "Unsupported daemon launch schema"
    );
    let instance_is_live = launch
        .instance_id
        .as_deref()
        .is_some_and(|instance| live_daemon_instance_matches(pid, instance));
    anyhow::ensure!(
        launch_authorizes(&launch, pid, instance_is_live),
        "Refusing stale daemon launch state"
    );
    Ok(launch)
}

#[derive(serde::Serialize, serde::Deserialize)]
struct RollbackPassphrase<S> {
    pid: u32,
    instance_id: Option<S>,
    passphrase: Option<S>,
}

fn retain_rollback_launch(
    _transaction: &crate::daemon::lifecycle::Transaction,
    launch: &ServeLaunch,
    passphrase: Option<&str>,
) -> Result<()> {
    anyhow::ensure!(
        launch.has_passphrase == passphrase.is_some(),
        "Incomplete rollback credentials"
    );
    let record = launch_file("serve.rollback.launch")?;
    let secret = launch_file("serve.rollback.passphrase")?;
    let credential = RollbackPassphrase {
        pid: launch.pid,
        instance_id: launch.instance_id.as_deref(),
        passphrase,
    };
    // Both slots must commit before stopping the still-valid current launch.
    secret.replace(&serde_json::to_vec(&credential)?)?;
    record.replace(&serde_json::to_vec_pretty(launch)?)
}

/// Consume the retained pair after successful restart or rollback readiness.
/// Removing credentials first makes partial consumption fail closed.
fn complete_rollback_after(result: Result<()>) -> Result<()> {
    result?;
    let launch = launch_file("serve.rollback.launch")?;
    let secret = launch_file("serve.rollback.passphrase")?;
    launch
        .read()?
        .context("Retained rollback launch disappeared before consumption")?;
    secret
        .read()?
        .context("Retained rollback credentials disappeared before consumption")?;
    secret
        .remove()
        .context("consuming retained rollback credentials")?;
    launch
        .remove()
        .context("consuming retained rollback launch")?;
    secret.sync_parent()?;
    launch.sync_parent()?;
    Ok(())
}

async fn rollback_daemon() -> Result<()> {
    let transaction = crate::daemon::lifecycle::Transaction::acquire().await?;
    let transaction = migrate_managed_launch(transaction, false).await?;
    let launch: ServeLaunch = serde_json::from_str(
        &launch_file("serve.rollback.launch")?
            .read()?
            .context("No retained daemon launch")?,
    )
    .context("Invalid retained daemon launch")?;
    anyhow::ensure!(
        launch.schema == SERVE_LAUNCH_SCHEMA,
        "Unsupported retained launch schema"
    );
    let credential: RollbackPassphrase<String> = serde_json::from_str(
        &launch_file("serve.rollback.passphrase")?
            .read()?
            .context("No retained daemon credentials")?,
    )
    .context("Invalid retained daemon credentials")?;
    anyhow::ensure!(
        credential.pid == launch.pid
            && credential.instance_id == launch.instance_id
            && credential.passphrase.is_some() == launch.has_passphrase,
        "Retained credentials do not match the retained launch",
    );
    let args = launch.to_serve_args(credential.passphrase);
    validate_launch_args(&launch.profile, &args).await?;
    match daemon_status() {
        DaemonStatus::Verified(pid) => {
            managed_launch(pid)?;
            stop_daemon_locked(&transaction).await?;
        }
        DaemonStatus::Unverified => bail!("Cannot verify the existing daemon; refusing rollback"),
        DaemonStatus::Absent => {}
    }
    let transaction = migrate_managed_launch(transaction, true).await?;
    complete_rollback_after(start_daemon(&launch.profile, &args, &transaction, true).await)
}

pub(crate) struct LaunchProfileUpdates(Vec<(crate::session::ResolvedDataFile, Vec<u8>)>);

impl LaunchProfileUpdates {
    pub(crate) fn prepare(
        _transaction: &crate::daemon::lifecycle::Transaction,
        old_profile: &str,
        new_profile: &str,
    ) -> Result<Self> {
        let mut updates = Vec::new();
        for name in ["serve.launch", "serve.rollback.launch"] {
            let file = launch_file(name)?;
            let Some(raw) = file.read()? else { continue };
            let mut launch: ServeLaunch =
                serde_json::from_str(&raw).with_context(|| format!("Invalid {name}"))?;
            if launch.profile == old_profile {
                new_profile.clone_into(&mut launch.profile);
                updates.push((file, serde_json::to_vec_pretty(&launch)?));
            }
        }
        Ok(Self(updates))
    }

    pub(crate) fn commit(self) -> Result<()> {
        for (file, content) in self.0 {
            file.replace(&content)?;
        }
        Ok(())
    }
}

/// Whether this launch record authorizes aoe to restart the process at `pid`.
/// A record written before instance IDs existed has only the PID to go on;
/// rejecting it would tell the user their daemon was externally launched, which
/// is both false and hides the `aoe serve --restart` that does work for it.
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

/// `true` when `serve.launch` claims this PID as our daemon but the live
/// process carries a different instance ID, which is the PID-recycle
/// signature. A missing launch record (a foreground `aoe serve` writes only
/// the PID file) or a record for a different PID is not a contradiction, so
/// stopping those still falls back to executable + subcommand verification.
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

fn recall_serve_passphrase(launch: &ServeLaunch) -> Result<Option<String>> {
    if !launch.has_passphrase {
        return Ok(None);
    }
    let passphrase = launch_file("serve.passphrase")?
        .read()?
        .or_else(|| std::env::var("AOE_SERVE_PASSPHRASE").ok())
        .context(
            "Cannot restart: the recorded passphrase is unavailable; leaving the daemon untouched",
        )?;
    Ok(Some(passphrase))
}

/// Resolve the passphrase for a launch that did not request one explicitly.
/// `recorded` is `None` when there is no predecessor policy to replay,
/// `Some(None)` when the predecessor ran without a passphrase, and
/// `Some(Some(_))` for its credential. An explicit request (Tunnel rotation)
/// wins, then the recorded policy, then the startup environment.
fn resolve_launch_passphrase(
    requested: Option<String>,
    recorded: Option<Option<&str>>,
    environment: impl FnOnce() -> Option<String>,
) -> Option<String> {
    requested.or_else(|| match recorded {
        Some(passphrase) => passphrase.map(str::to_owned),
        None => environment(),
    })
}

/// One URL we can show in the Active state. Tunnel mode has a public primary
/// plus a loopback alternate for same-host clients. Local mode may have
/// multiple entries (Tailscale + LAN + localhost), and the user can Tab-cycle
/// between them.
#[derive(Debug, Clone)]
pub struct ServeUrl {
    /// Optional human-readable label ("tailscale", "lan", "localhost").
    /// None for the primary URL, which does not need one.
    pub label: Option<String>,
    pub url: String,
}

/// Read `$APP_DIR/serve.url`. Returns `[]` when the file is missing or
/// empty. The primary URL gets `label: None` for rendering; alternates
/// carry their label.
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
            // Primary line is the bare URL.
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
            // Defensive: unlabeled extra line. Show as a nameless extra.
            out.push(ServeUrl {
                label: None,
                url: line.to_string(),
            });
        }
    }
    out
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

    // The top-level globals declared on `Cli` in `src/cli/definition.rs`, so a
    // value like `--profile work` is not mistaken for the subcommand.
    // `parser_knows_every_top_level_global` fails if that list changes.
    let mut index = 1;
    while let Some(arg) = args.get(index) {
        match *arg {
            b"-p" | b"--profile" | b"--daemon-url" => {
                index += 2;
                if index > args.len() {
                    return false;
                }
            }
            b"--no-autostart" => index += 1,
            arg if arg.starts_with(b"--profile=")
                || arg.starts_with(b"--no-autostart=")
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

/// Cross-platform check that `pid` belongs to an `aoe serve` process.
/// PIDs get recycled, so a successful signal probe alone cannot authorize
/// process control.
fn inspect_daemon_process(pid: i32) -> DaemonProcessIdentity {
    // Linux fast path: read /proc directly, no subprocess.
    let proc_path = format!("/proc/{}/cmdline", pid);
    if std::path::Path::new(&proc_path).exists() {
        return match std::fs::read(&proc_path) {
            Ok(cmdline) if command_is_aoe_serve(&cmdline) => DaemonProcessIdentity::Verified,
            Ok(_) => DaemonProcessIdentity::Foreign,
            Err(_) => DaemonProcessIdentity::Indeterminate,
        };
    }

    // macOS / other: shell out to `ps`. `-o command=` prints the full
    // command (path + args) with no header, and `-ww` stops it being clipped
    // at the terminal width before `serve` shows up.
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

/// Loose companion to `command_is_aoe_serve`, for the `ps` path only. `ps`
/// joins argv with spaces, so an executable path or option value that itself
/// contains a space cannot be split back into argv and the strict parse fails
/// on a genuine daemon. When the raw output still names an aoe executable,
/// report `Indeterminate` rather than `Foreign`: the two costs are not
/// symmetric, since `Foreign` deletes `serve.launch` and `serve.passphrase`
/// and so throws away the only way to restart a daemon that is still running.
/// A trailing space is required so a mention in an unrelated argument
/// (`vim src/aoe.rs`) still classifies as foreign.
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

const SERVE_STATE_FILES: [&str; 4] = [
    "serve.url",
    "serve.mode",
    "serve.passphrase",
    "serve.launch",
];

/// Separate from the lifetime lock a running server holds, which this state is
/// published and cleared under. Publishers hold it from before spawning until the pid
/// and launch files are written; cleanup holds it across its ownership check and deletion.
pub(crate) fn lock_serve_state() -> Result<std::fs::File> {
    use fs2::FileExt as _;
    let root = crate::session::AnchoredDir::open(&crate::session::get_app_dir()?)?;
    let (file, path) = root.open_lock("serve.state.lock".as_ref())?;
    crate::process::validate_recovery_file(&file)?;
    file.lock_exclusive()
        .with_context(|| format!("locking {}", path.display()))?;
    Ok(file)
}

/// Deletes the serve state only while `serve.pid` still names `expected_pid`.
/// Returns whether it did.
fn clear_serve_state(expected_pid: i32) -> bool {
    let (Ok(_lock), Ok(pid_path), Ok(dir)) = (
        lock_serve_state(),
        pid_file_path(),
        crate::session::get_app_dir(),
    ) else {
        return false;
    };
    let recorded = std::fs::read_to_string(&pid_path)
        .ok()
        .and_then(|raw| parse_positive_pid(&raw));
    if recorded != Some(expected_pid) {
        return false;
    }
    let _ = std::fs::remove_file(&pid_path);
    for name in SERVE_STATE_FILES {
        let _ = std::fs::remove_file(dir.join(name));
    }
    true
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DaemonStatus {
    Absent,
    Verified(u32),
    Unverified,
}

pub(crate) fn daemon_status() -> DaemonStatus {
    daemon_status_with(
        |pid| {
            classify_daemon_probe(nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid),
                None,
            ))
        },
        inspect_daemon_process,
    )
}

fn daemon_status_with(
    probe: impl FnOnce(i32) -> DaemonProbeDisposition,
    inspect: impl FnOnce(i32) -> DaemonProcessIdentity,
) -> DaemonStatus {
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

    match probe(pid) {
        DaemonProbeDisposition::VerifyIdentity => match inspect(pid) {
            DaemonProcessIdentity::Verified => DaemonStatus::Verified(pid as u32),
            DaemonProcessIdentity::Foreign => DaemonStatus::Absent,
            DaemonProcessIdentity::Indeterminate => DaemonStatus::Unverified,
        },
        DaemonProbeDisposition::Stale => DaemonStatus::Absent,
        DaemonProbeDisposition::Indeterminate => DaemonStatus::Unverified,
    }
}

/// Verifying the daemon's identity spawns `ps`, so keep calls off the render path.
pub fn daemon_pid() -> Option<u32> {
    match daemon_status() {
        DaemonStatus::Verified(pid) => Some(pid),
        DaemonStatus::Absent | DaemonStatus::Unverified => None,
    }
}

/// The verified daemon's recorded launch, which a TUI restart replays.
pub(crate) fn recorded_launch() -> Option<ServeLaunch> {
    managed_launch(daemon_pid()?).ok()
}

/// Whether the verified daemon already runs `passphrase`.
pub(crate) fn daemon_runs_passphrase(passphrase: &str) -> bool {
    recorded_launch()
        .and_then(|launch| recall_serve_passphrase(&launch).ok().flatten())
        .is_some_and(|running| running == passphrase)
}

/// Returns a verified daemon only when it exposes the web dashboard.
pub(crate) fn web_daemon_pid() -> Option<u32> {
    let pid = daemon_pid()?;
    (!managed_launch(pid).is_ok_and(|launch| launch.core_only)).then_some(pid)
}

async fn validate_launch_args(profile: &str, args: &ServeArgs) -> Result<AuthMode> {
    crate::session::require_known_profile(profile)?;
    let auth_mode = if args.core_only {
        AuthMode::None
    } else {
        resolve_auth_mode(args.auth, args.no_auth)
    };
    validate_auth_combination(
        auth_mode,
        args.passphrase.is_some(),
        host_is_localhost(&args.host),
        args.behind_proxy,
        args.remote,
        &args.host,
    )?;
    validate_behind_proxy_allowlist(args.behind_proxy, args.remote, &args.allowed_host)?;
    validate_allowed_hosts(&args.allowed_host)?;
    validate_allowed_origins(&args.allowed_origin)?;
    if args.tunnel_name.is_some() && args.tunnel_url.is_none() {
        bail!("Named tunnels require --tunnel-url to specify the hostname");
    }
    if args.remote {
        if args.passphrase.is_none() {
            bail!("Remote exposure requires --passphrase or AOE_SERVE_PASSPHRASE");
        }
        let tailscale_ok =
            tokio::task::spawn_blocking(crate::server::tunnel::tailscale_available_sync).await?;
        if cloudflared_required(args.no_tailscale, args.tunnel_name.is_some(), tailscale_ok) {
            tokio::task::spawn_blocking(crate::server::tunnel::check_cloudflared).await??;
        }
    }
    Ok(auth_mode)
}

/// Dispatch lifecycle controls before startup migrations and profile validation.
pub async fn run_control(args: &ServeArgs) -> Option<Result<()>> {
    if args.stop {
        Some(stop_daemon().await)
    } else if args.status {
        Some(print_status().await)
    } else if args.restart {
        Some(restart_daemon().await)
    } else if args.rollback {
        Some(rollback_daemon().await)
    } else {
        None
    }
}
#[tracing::instrument(target = "cli.serve", skip_all, fields(profile = %profile))]
pub async fn run(profile: &str, mut args: ServeArgs, startup: Option<ServeStartup>) -> Result<()> {
    let startup = match startup {
        Some(startup) => startup,
        None => lock_for_startup(profile, &mut args)
            .await?
            .context("serve startup guard missing")?,
    };
    let ServeStartup {
        lock: serve_lock,
        transaction,
    } = startup;

    let auth_mode = validate_launch_args(profile, &args).await?;
    let is_localhost = host_is_localhost(&args.host);

    if args.behind_proxy && args.remote {
        let msg = "--behind-proxy is ignored when --remote is set; \
             --remote already enables the equivalent cookie-Secure and \
             trusted-XFF behavior and manages its own ingress.";
        eprintln!("Note: {msg}");
        tracing::warn!(target: "serve", "{msg}");
    }

    let host = if args.remote {
        "127.0.0.1".to_string()
    } else {
        args.host.clone()
    };

    // Warn about security implications of network binding (non-remote, non-localhost)
    if !is_localhost && !args.remote {
        eprintln!("==========================================================");
        eprintln!("  SECURITY WARNING: Binding to {}", args.host);
        eprintln!("==========================================================");
        eprintln!();
        eprintln!("  This exposes terminal access to your network.");
        eprintln!("  Anyone with the auth token can execute commands");
        eprintln!("  as your user on this machine.");
        eprintln!();
        if args.dangerously_allow_plaintext_remote {
            eprintln!("  Traffic is NOT encrypted (HTTP, not HTTPS), and");
            eprintln!("  --dangerously-allow-plaintext-remote lets remote clients");
            eprintln!("  send credentials in the clear.");
        } else {
            eprintln!("  Plain-HTTP requests from other machines are refused");
            eprintln!("  (426). Put a TLS proxy in front of a loopback bind");
            eprintln!("  (--behind-proxy), or pass");
            eprintln!("  --dangerously-allow-plaintext-remote to accept them.");
        }
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
        // A wildcard bind trusts loopback + any routable IP literal (which
        // cannot be DNS-rebound), so the by-IP URLs printed below work as-is.
        // Only access by a HOSTNAME/mDNS name still needs an allowlist entry.
        // See #2735.
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

    // Passphrase strength check
    if let Some(ref passphrase) = args.passphrase {
        if let Some(warning) = crate::server::login::check_passphrase_strength(passphrase) {
            eprintln!("{}", warning);
            eprintln!();
        }
    }

    if args.daemon {
        drop(serve_lock);
        return start_daemon(profile, &args, &transaction, true).await;
    }
    let _serve_lock = serve_lock;

    let _lifetime = transaction.lifetime()?;
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

    // Write PID file for non-daemon mode too (so --stop works either way)
    if let Ok(path) = pid_file_path() {
        let _lock = lock_serve_state()?;
        let _ = std::fs::write(&path, std::process::id().to_string());
        tracing::debug!(target: "serve.lifecycle", path = %path.display(), pid = std::process::id(), "wrote pid file");
    }

    let result = crate::server::start_server(
        crate::server::ServerConfig {
            profile,
            core_only: args.core_only,
            host: &host,
            port: args.resolved_port(),
            auth_mode,
            read_only: args.read_only,
            remote: args.remote,
            tunnel_name: args.tunnel_name.as_deref(),
            tunnel_url: args.tunnel_url.as_deref(),
            no_tailscale: args.no_tailscale,
            is_daemon: args.daemon_child,
            passphrase: args.passphrase.as_deref(),
            behind_proxy: args.behind_proxy,
            allow_plaintext_remote: args.dangerously_allow_plaintext_remote,
            open_browser: args.open,
            extra_allowed_hosts: args.allowed_host.clone(),
            extra_allowed_origins: args.allowed_origin.clone(),
        },
        transaction,
    )
    .await;

    clear_serve_state(std::process::id() as i32);

    result
}

/// Path the daemon's stdout/stderr are redirected to. Resolved from the
/// configured `[logging].file_path` so panic backtraces interleave with
/// the structured tracing stream. Used by `start_daemon()` for the stdio
/// redirect, by the TUI serve dialog for the tail pane, and by `aoe logs`
/// for the viewer target.
pub fn stdio_redirect_path() -> Result<PathBuf> {
    let dir = crate::session::get_app_dir()?;
    let log_cfg = crate::session::load_config()
        .ok()
        .flatten()
        .map(|c| c.logging)
        .unwrap_or_default();
    Ok(crate::logging::resolve_log_path(&log_cfg, &dir))
}

/// URL a CityHall-hosted workspace fetches its config bundle from. Unset means
/// this install is not CityHall-managed and the fetch is skipped entirely.
const BUNDLE_URL_ENV: &str = "AOE_CITYHALL_BUNDLE_URL";
/// Bearer token for [`BUNDLE_URL_ENV`]. CityHall issues one per workspace.
const BUNDLE_TOKEN_ENV: &str = "AOE_CITYHALL_BUNDLE_TOKEN";
/// The bundle is small and the workspace cannot serve until it lands, so a short
/// ceiling is better than a slow start behind an unresponsive CityHall.
const BUNDLE_FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// Fetch and apply the CityHall config bundle when this install is pointed at
/// one. See `crate::session::cityhall_bundle`.
///
/// Failure is deliberately asymmetric:
///
/// - **No cached bundle** means this is a first boot. Continuing would leave the
///   user in a workspace with default settings and no projects, which is the
///   exact confusion the bundle exists to remove, so the boot fails loudly and
///   CityHall surfaces it as a workspace that would not start.
/// - **A cached bundle exists**, so the workspace is already configured. A
///   transient CityHall outage must not brick it, so this warns and serves with
///   what is on disk.
///
/// A bundle that arrives but is malformed or carries an unknown settings key is
/// fatal either way: that is an admin error, and swallowing it would leave the
/// admin believing a broken document had been applied.
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
            return Err(e.context(
                "fetching the CityHall config bundle. The workspace has no cached configuration, so it cannot start.",
            ))
        }
    };

    let bundle = CityHallBundle::from_toml(&raw)
        .map_err(|_| anyhow::anyhow!("Invalid CityHall config bundle"))?;
    let report = cityhall_bundle::apply(&bundle)?;
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

    // Cache last: a document that failed to apply must not be remembered as the
    // configuration this workspace is running.
    if let Err(e) = std::fs::write(&cache, &raw) {
        tracing::warn!(target: "serve.cityhall", error = %e, "could not cache the bundle");
    }
    Ok(())
}

async fn fetch_cityhall_bundle(url: &str, token: &str) -> Result<String> {
    use crate::daemon::{self, ApiErrorCode, DaemonClientError};
    let url = daemon::native_url(url)?;
    let authorization = daemon::authorization_header((!token.is_empty()).then_some(token))?;
    let client = daemon::native_http_client(&url, authorization.is_some())?;
    let mut request = client.get(url).timeout(BUNDLE_FETCH_TIMEOUT);
    if let Some(authorization) = authorization {
        request = request.header(reqwest::header::AUTHORIZATION, authorization);
    }
    let request = request.build().map_err(|_| DaemonClientError::Transport)?;
    let response = daemon::transport::execute(&client, None, request).await?;
    let status = response.status();
    if !status.is_success() {
        return Err(DaemonClientError::Status {
            status,
            code: ApiErrorCode::from_headers(status, response.headers(), false),
            body: String::new(),
            truncated: false,
        }
        .into());
    }
    Ok(daemon::decode_text(response).await?)
}

pub(crate) async fn ensure_local_daemon(
    profile: &str,
    allow_start: bool,
) -> Result<(crate::acp::client::DaemonEndpoint, Option<u32>)> {
    use clap::Parser;
    let transaction = crate::daemon::lifecycle::Transaction::acquire().await?;
    let existing = match daemon_status() {
        DaemonStatus::Verified(pid) => {
            anyhow::ensure!(
                transaction.try_lifetime()?.is_none(),
                "Running daemon (PID {pid}) predates this build's local runtime transport. \
                 Run `aoe serve --restart` with the new binary, or stop it through its manager"
            );
            Some(pid)
        }
        DaemonStatus::Unverified => {
            bail!("Cannot verify the existing daemon; refusing local runtime startup")
        }
        DaemonStatus::Absent => None,
    };
    let started = if existing.is_none() {
        anyhow::ensure!(
            allow_start,
            "Local daemon autostart is disabled; start one with `aoe serve --core-only --daemon`"
        );
        let resolved_profile = crate::session::config::effective_profile(profile);
        let profile = resolved_profile.as_str();
        crate::session::require_known_profile(profile)?;
        let cli = crate::cli::Cli::try_parse_from(["aoe", "serve", "--core-only", "--daemon"])?;
        let Some(crate::cli::Commands::Serve(args)) = cli.command else {
            unreachable!()
        };
        start_daemon(profile, &args, &transaction, false).await?;
        daemon_pid()
    } else {
        None
    };
    let endpoint = crate::acp::client::DaemonEndpoint::local_unix(
        crate::daemon::transport::local_socket_path()?,
    );
    endpoint
        .daemon_client()?
        .local_runtime_ready((!profile.is_empty()).then_some(profile))
        .await?;
    Ok((endpoint, started))
}
pub(crate) async fn start_dashboard_for_tui(
    profile: &str,
    mut args: ServeArgs,
    predecessor_epoch: Option<&str>,
    restart: bool,
) -> Result<(
    crate::daemon::RuntimeConnection,
    crate::daemon::DaemonClient,
)> {
    let launch = if restart {
        restart_daemon_with_receipt(Some(args), false).await?
    } else {
        let startup = lock_for_startup(profile, &mut args)
            .await?
            .context("Dashboard startup guard is unavailable")?;
        let startup = tokio::task::spawn_blocking(move || {
            startup.run_migrations(None)?;
            Ok::<_, anyhow::Error>(startup)
        })
        .await??;
        let ServeStartup { lock, transaction } = startup;
        validate_launch_args(profile, &args).await?;
        drop(lock);
        start_daemon(profile, &args, &transaction, false).await?;
        managed_launch(daemon_pid().context("Dashboard successor is unavailable")?)?
    };
    anyhow::ensure!(!launch.core_only, "Successor did not enable the dashboard");
    let instance = launch
        .instance_id
        .context("Successor instance proof is missing")?;
    let endpoint = crate::acp::client::DaemonEndpoint::local_unix(
        crate::daemon::transport::local_socket_path()?,
    );
    let connection = crate::daemon::RuntimeConnection::connect(
        &endpoint,
        (!profile.is_empty()).then_some(profile),
    )
    .await?;
    anyhow::ensure!(
        connection.info().owner_instance_id.as_deref() == Some(instance.as_str()),
        "Dashboard successor identity changed before reconnect"
    );
    anyhow::ensure!(
        predecessor_epoch.is_none_or(|epoch| epoch != connection.info().epoch),
        "Dashboard successor reused the predecessor epoch"
    );
    let client = endpoint.daemon_client()?;
    Ok((connection, client))
}

async fn start_daemon(
    profile: &str,
    args: &ServeArgs,
    transaction: &crate::daemon::lifecycle::Transaction,
    announce: bool,
) -> Result<()> {
    use std::process::{Command, Stdio};

    let resolved_profile = profile
        .is_empty()
        .then(crate::session::config::resolve_default_profile);
    let profile = resolved_profile.as_deref().unwrap_or(profile);

    let _state_lock = lock_serve_state()?;
    if let Some(existing) = daemon_pid() {
        bail!("aoe serve daemon already running (PID {existing}); not starting another");
    }
    let exe = std::env::current_exe()?;
    let instance_id = uuid::Uuid::new_v4().to_string();
    let mut cmd = Command::new(exe);
    cmd.env(SERVE_INSTANCE_ENV, &instance_id);
    cmd.env_remove("AOE_CITYHALL_MODE")
        .env_remove("AOE_SERVE_PASSPHRASE");
    cmd.args(["serve", "--daemon-child"]);
    if args.core_only {
        cmd.arg("--core-only");
    } else {
        cmd.args([
            "--port",
            &args.resolved_port().to_string(),
            "--host",
            &args.host,
        ]);
    }

    if args.no_auth {
        cmd.arg("--no-auth");
    }
    if let Some(mode) = args.auth {
        cmd.args(["--auth", mode.as_cli_str()]);
    }
    if args.behind_proxy {
        cmd.arg("--behind-proxy");
    }
    if args.dangerously_allow_plaintext_remote {
        cmd.arg("--dangerously-allow-plaintext-remote");
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
        // Pass via env var to avoid exposing the passphrase in the process list
        cmd.env("AOE_SERVE_PASSPHRASE", passphrase);
    }
    if !profile.is_empty() {
        cmd.args(["--profile", profile]);
    }

    cmd.stdin(transaction.handoff()?);

    // Create a new session so the daemon is not killed by SIGHUP when the
    // parent terminal closes. setsid() is async-signal-safe.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid() is async-signal-safe per POSIX, which is the
        // only requirement for pre_exec closures.
        unsafe {
            cmd.pre_exec(|| {
                nix::unistd::setsid().map_err(std::io::Error::other)?;
                Ok(())
            });
        }
    }

    // Route the child's stdout/stderr into the configured log file so panic
    // backtraces and stray prints land alongside structured tracing rather
    // than disappearing into /dev/null. The tracing subscriber inside the
    // child resolves the same path via `logging::resolve_log_path`, so the
    // two streams interleave in one file. Inherited fds may go stale across
    // a rotation; that is best-effort behavior.
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

    let mut child = cmd.spawn()?;
    let pid = child.id();
    let birth = match crate::process::process_birth_identity(pid)
        .and_then(|birth| birth.context("Spawned daemon has no provable kernel birth"))
    {
        Ok(birth) => birth,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
    };

    tracing::info!(
        target: "serve.daemon",
        pid,
        profile = %profile,
        port = args.resolved_port(),
        host = %args.host,
        remote = args.remote,
        "daemon child spawned",
    );

    let launch = ServeLaunch {
        schema: SERVE_LAUNCH_SCHEMA,
        pid,
        instance_id: Some(instance_id),
        profile: profile.to_string(),
        core_only: args.core_only,
        host: args.host.clone(),
        port: args.resolved_port(),
        auth_mode: if args.core_only {
            AuthMode::None
        } else {
            resolve_auth_mode(args.auth, args.no_auth)
        },
        has_passphrase: args.passphrase.is_some(),
        behind_proxy: args.behind_proxy,
        allow_plaintext_remote: args.dangerously_allow_plaintext_remote,
        read_only: args.read_only,
        cityhall: args.cityhall,
        remote: args.remote,
        tunnel_name: args.tunnel_name.clone(),
        tunnel_url: args.tunnel_url.clone(),
        no_tailscale: args.no_tailscale,
        allowed_host: args.allowed_host.clone(),
        allowed_origin: args.allowed_origin.clone(),
        additional_fields: serde_json::Map::new(),
    };
    if let Err(error) = write_serve_launch(&launch) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    drop(_state_lock);
    let endpoint = crate::acp::client::DaemonEndpoint::local_unix(
        crate::daemon::transport::local_socket_path()?,
    );
    let client = endpoint.daemon_client()?;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
    let ready = tokio::time::timeout_at(deadline, async {
        #[cfg(target_os = "linux")]
        let mut require_local_read = None;
        loop {
            if let Some(status) = child.try_wait()? {
                anyhow::bail!("Daemon exited before readiness: {status}");
            }
            if client.local_daemon_ready(pid).await.unwrap_or(false)
                && daemon_pid() == Some(pid)
                && launch
                    .instance_id
                    .as_deref()
                    .is_some_and(|instance| live_daemon_instance_matches(pid, instance))
                && crate::process::process_birth_identity(pid)
                    .is_ok_and(|current| current.as_ref() == Some(&birth))
            {
                #[cfg(target_os = "linux")]
                {
                    use crate::cli::runtime_read::uds::{self, TrustedPathError};
                    let required = *require_local_read.get_or_insert_with(|| {
                        !args.cityhall
                            && match uds::existing_app_namespace() {
                                Ok(_) | Err(TrustedPathError::Missing) => true,
                                Err(TrustedPathError::Invalid | TrustedPathError::Refused(_)) => {
                                    false
                                }
                            }
                    });
                    if required
                        && !uds::connect(deadline)
                            .await
                            .is_ok_and(|connection| connection.owned_by(pid))
                    {
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        continue;
                    }
                }
                return Ok::<(), anyhow::Error>(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await;
    match ready {
        Ok(Ok(())) => {}
        result => {
            let _ = child.kill();
            let _ = child.wait();
            return match result {
                Ok(Err(error)) => Err(error),
                Err(_) => Err(anyhow::anyhow!("Daemon readiness timed out")),
                Ok(Ok(())) => unreachable!(),
            };
        }
    }

    if announce {
        println!("aoe serve started as daemon (PID {})", pid);
        println!("Stop with: aoe serve --stop");
    }
    Ok(())
}

async fn migrate_managed_launch(
    transaction: crate::daemon::lifecycle::Transaction,
    full: bool,
) -> Result<crate::daemon::lifecycle::Transaction> {
    tokio::task::spawn_blocking(move || {
        crate::migrations::ensure_schema_supported()?;
        if full {
            let serve = try_lock_serve()?.context("serve or migration is still running")?;
            crate::migrations::run_migrations_under_exclusion(
                Some(crate::cli::migrate::stderr_reporter()),
                false,
                &transaction,
                &serve,
            )?;
        } else {
            crate::migrations::reconcile_serve_launch_under_transaction(&transaction)?;
        }
        Ok(transaction)
    })
    .await?
}

/// Replay the managed launch with the current executable, retaining it before stop.
#[tracing::instrument(target = "serve.lifecycle", skip_all)]
pub async fn restart_daemon() -> Result<()> {
    restart_daemon_with_receipt(None, true).await.map(|_| ())
}

pub(crate) async fn restart_daemon_with_receipt(
    override_args: Option<ServeArgs>,
    announce: bool,
) -> Result<ServeLaunch> {
    let transaction = crate::daemon::lifecycle::Transaction::acquire().await?;
    let Some(pid) = daemon_pid() else {
        bail!(
            "No running aoe serve daemon to restart.\n\
             Start one with: aoe serve --daemon"
        );
    };
    let transaction = migrate_managed_launch(transaction, false).await?;

    let launch = managed_launch(pid)?;
    let predecessor_passphrase = recall_serve_passphrase(&launch)?;
    let mut args = match override_args {
        Some(requested) => launch.overlay_tui_args(requested),
        None => launch.to_serve_args(None),
    };
    args.passphrase = resolve_launch_passphrase(
        args.passphrase.take(),
        Some(predecessor_passphrase.as_deref()),
        || None,
    );
    validate_launch_args(&launch.profile, &args).await?;
    retain_rollback_launch(&transaction, &launch, predecessor_passphrase.as_deref())?;

    if announce {
        println!("Restarting aoe serve daemon (PID {pid})…");
    }
    stop_daemon_locked(&transaction).await?;
    let transaction = migrate_managed_launch(transaction, true).await?;
    complete_rollback_after(start_daemon(&launch.profile, &args, &transaction, announce).await)?;
    managed_launch(daemon_pid().context("Restart successor is unavailable")?)
}

fn report_stale_cleanup(pid: i32) {
    if clear_serve_state(pid) {
        println!("Daemon was not running (stale PID file cleaned up)");
    } else {
        println!("Daemon was not running; serve state changed meanwhile and was preserved");
    }
}

#[tracing::instrument(target = "serve.shutdown", skip_all)]
pub(crate) async fn stop_daemon() -> Result<()> {
    let transaction = crate::daemon::lifecycle::Transaction::acquire().await?;
    stop_daemon_locked(&transaction).await
}

async fn stop_daemon_locked(transaction: &crate::daemon::lifecycle::Transaction) -> Result<()> {
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
    if matches!(
        classify_daemon_probe(nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid),
            None
        )),
        DaemonProbeDisposition::Stale
    ) {
        report_stale_cleanup(pid);
        return Ok(());
    }

    // Capture before inspecting custody, so process exit or PID reuse during
    // inspection can never redirect a later signal to another incarnation.
    let birth = crate::process::process_birth_identity(pid as u32)?
        .context("Daemon exited before its physical identity could be captured; retry stop")?;
    let process = crate::process::CapturedProcess::capture(pid as u32, &birth)?
        .context("Daemon incarnation changed before shutdown; preserving serve state")?;
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
            if clear_serve_state(pid) {
                bail!("PID {pid} belongs to a different process (stale PID file). Cleaned up.");
            }
            bail!(
                "PID {pid} belongs to a different process; serve state changed meanwhile and was preserved."
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
    anyhow::ensure!(
        crate::process::process_birth_identity(pid as u32)?.as_ref() == Some(&birth)
            && !process.exited()?,
        "Daemon incarnation changed during authorization; preserving serve state"
    );
    let lifetime_owned = transaction.try_lifetime()?.is_none();
    tracing::info!(target: "serve.shutdown", pid, "sending SIGTERM to daemon");
    process.signal(nix::libc::SIGTERM)?;
    let term_deadline = std::time::Instant::now() + std::time::Duration::from_secs(7);
    let mut kill_deadline = None;
    while !process.exited()? {
        let now = std::time::Instant::now();
        match kill_deadline {
            Some(deadline) => anyhow::ensure!(
                now < deadline,
                "Daemon physical exit remains unproven after shutdown; preserving serve state"
            ),
            None if now >= term_deadline => {
                // Authority stays with the handle captured before SIGTERM.
                // A zombie has exited even while kill(pid, 0) still succeeds.
                process.signal(nix::libc::SIGKILL)?;
                kill_deadline = Some(now + std::time::Duration::from_secs(2));
            }
            None => {}
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    if lifetime_owned {
        anyhow::ensure!(
            transaction.try_lifetime()?.is_some(),
            "Daemon retained lifetime ownership after physical exit; preserving serve state"
        );
    }
    clear_serve_state(pid);
    println!("Stopped aoe serve daemon (PID {})", pid);
    Ok(())
}

/// Print the running daemon's PID, mode, URLs, and log path. Exits
/// non-zero (via `bail!`) when no daemon is running so shell scripts
/// can branch on it (`aoe serve --status && …`).
async fn print_status() -> Result<()> {
    // `AOE_DAEMON_URL` retargets every `aoe` invocation at a remote
    // daemon (see docs/acp.md). `--status` follows the same rule:
    // when the env override is set, report the remote endpoint's
    // health instead of the local PID file.
    if let Some(endpoint) = crate::acp::client::discovery::discover_env() {
        let client = crate::acp::client::HttpClient::new(endpoint.clone())
            .map_err(|e| anyhow::anyhow!("http client init failed: {e}"))?;
        match client.health_check().await {
            Ok(()) => {
                println!("Daemon: reachable (remote via AOE_DAEMON_URL)");
                println!(
                    "Token:  {}",
                    if endpoint.has_token() { "set" } else { "unset" }
                );
                Ok(())
            }
            Err(e) => bail!(
                "AOE_DAEMON_URL is set but the daemon is unreachable ({e}); check the address or unset to use a local daemon"
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
    // Resolve the configured log path (default debug.log under app_dir).
    // The daemon's tracing and stdout/stderr both land here post-consolidation;
    // `serve.log` is retired.
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

/// Cached label of the running daemon's serve mode, refreshed by pid.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cityhall_refusal_cannot_reflect_the_bearer_into_errors() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let token = "cityhall-secret-for-reflection";
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            let read = stream.read(&mut request).await.unwrap();
            assert!(read > 0, "the client must send its request");
            stream.write_all(format!("HTTP/1.1 403 Forbidden\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{token}", token.len()).as_bytes()).await.unwrap();
        });
        let error = fetch_cityhall_bundle(&format!("http://{address}/api/cityhall/bundle"), token)
            .await
            .unwrap_err();
        server.await.unwrap();
        assert!(!format!("{error:#} {error:?}").contains(token));
        assert!(
            matches!(error.downcast_ref::<crate::daemon::DaemonClientError>(),
            Some(crate::daemon::DaemonClientError::Status { status, .. }) if *status == reqwest::StatusCode::FORBIDDEN)
        );
    }

    #[test]
    fn cloudflared_skipped_when_tailscale_available_and_default_flags() {
        // Regression: aoe serve --remote with Tailscale up and cloudflared
        // missing was failing because of the unconditional check. Tailscale
        // alone is enough.
        assert!(!cloudflared_required(false, false, true));
    }

    #[test]
    fn cloudflared_required_when_no_tailscale_flag_set() {
        assert!(cloudflared_required(true, false, true));
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
            (b"aoe --no-autostart serve --daemon", true),
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
    fn serve_and_migrate_exclude_each_other_in_both_orders() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("serve.lock");
        let daemon = try_lock_serve_at(&path).unwrap().expect("first holder");
        assert!(try_lock_serve_at(&path).unwrap().is_none());
        drop(daemon);
        let migrate = try_lock_serve_at(&path)
            .unwrap()
            .expect("free after release");
        assert!(try_lock_serve_at(&path).unwrap().is_none());
        drop(migrate);
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
    fn cloudflared_required_when_named_tunnel_pinned() {
        assert!(cloudflared_required(false, true, true));
    }

    #[test]
    fn cloudflared_required_when_tailscale_unavailable() {
        assert!(cloudflared_required(false, false, false));
    }

    #[test]
    fn host_is_localhost_accepts_loopback_forms() {
        assert!(host_is_localhost("localhost"));
        assert!(host_is_localhost("127.0.0.1"));
        assert!(host_is_localhost("::1"));
    }

    #[test]
    fn host_is_localhost_rejects_routable_addresses() {
        assert!(!host_is_localhost("0.0.0.0"));
        assert!(!host_is_localhost("192.168.1.1"));
        assert!(!host_is_localhost("aoe.example.com"));
    }

    #[test]
    fn resolve_auth_mode_defaults_to_token() {
        assert_eq!(resolve_auth_mode(None, false), AuthMode::Token);
    }

    #[test]
    fn resolve_auth_mode_no_auth_alias_maps_to_none() {
        assert_eq!(resolve_auth_mode(None, true), AuthMode::None);
    }

    #[test]
    fn resolve_auth_mode_explicit_wins() {
        assert_eq!(
            resolve_auth_mode(Some(AuthMode::Passphrase), false),
            AuthMode::Passphrase
        );
        assert_eq!(
            resolve_auth_mode(Some(AuthMode::None), false),
            AuthMode::None
        );
    }

    #[test]
    fn validate_token_mode_loopback_ok() {
        assert!(
            validate_auth_combination(AuthMode::Token, false, true, false, false, "127.0.0.1")
                .is_ok()
        );
    }

    #[test]
    fn validate_passphrase_without_passphrase_fails() {
        let err =
            validate_auth_combination(AuthMode::Passphrase, false, true, false, false, "127.0.0.1")
                .unwrap_err();
        assert!(err.to_string().contains("--auth=passphrase requires"));
    }

    #[test]
    fn validate_passphrase_with_passphrase_loopback_ok() {
        assert!(validate_auth_combination(
            AuthMode::Passphrase,
            true,
            true,
            false,
            false,
            "127.0.0.1"
        )
        .is_ok());
    }

    #[test]
    fn validate_none_with_passphrase_rejected() {
        let err = validate_auth_combination(AuthMode::None, true, true, false, false, "127.0.0.1")
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("--auth=none does not honor --passphrase"));
        assert!(msg.contains("--auth=passphrase"));
    }

    #[test]
    fn validate_passphrase_non_loopback_needs_behind_proxy() {
        let err =
            validate_auth_combination(AuthMode::Passphrase, true, false, false, false, "0.0.0.0")
                .unwrap_err();
        assert!(err.to_string().contains("--behind-proxy"));
    }

    #[test]
    fn validate_passphrase_non_loopback_with_behind_proxy_ok() {
        assert!(validate_auth_combination(
            AuthMode::Passphrase,
            true,
            false,
            true,
            false,
            "0.0.0.0"
        )
        .is_ok());
    }

    #[test]
    fn validate_none_non_loopback_needs_behind_proxy() {
        let err = validate_auth_combination(AuthMode::None, false, false, false, false, "0.0.0.0")
            .unwrap_err();
        assert!(err.to_string().contains("--behind-proxy"));
    }

    #[test]
    fn validate_none_loopback_ok() {
        // Regression: --no-auth (== --auth=none) on loopback must still
        // start, matching the legacy --no-auth behavior.
        assert!(
            validate_auth_combination(AuthMode::None, false, true, false, false, "127.0.0.1")
                .is_ok()
        );
    }

    #[test]
    fn validate_passphrase_with_remote_rejected() {
        let err =
            validate_auth_combination(AuthMode::Passphrase, true, true, false, true, "127.0.0.1")
                .unwrap_err();
        assert!(err.to_string().contains("in remote mode"));
    }

    #[test]
    fn validate_none_with_remote_rejected() {
        let err = validate_auth_combination(AuthMode::None, false, true, false, true, "127.0.0.1")
            .unwrap_err();
        assert!(err.to_string().contains("in remote mode"));
    }

    #[test]
    fn validate_token_with_remote_ok() {
        // --remote requires token + passphrase; the passphrase requirement
        // is enforced separately. Token + remote alone is the existing
        // valid combination and must keep passing.
        assert!(
            validate_auth_combination(AuthMode::Token, true, true, false, true, "127.0.0.1")
                .is_ok()
        );
    }

    #[test]
    fn auth_mode_cli_str_matches_clap() {
        // Drift guard: `as_cli_str()` and clap's `value(rename_all =
        // "lowercase")` derive must agree. If someone renames a variant
        // or changes the rename_all rule without updating the match,
        // this round-trip fails. Catches the silent split where
        // `--auth=passphrase` parses but the daemon respawn emits
        // `--auth Passphrase`.
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
        }
    }

    #[test]
    fn auth_mode_serde_matches_cli_str() {
        // serve.launch persists the auth mode as JSON; the serde
        // representation must match clap's `--auth=<mode>` spelling so a
        // restart replays the same flag. Guards the serde `rename_all`
        // against drift from `as_cli_str()`.
        for variant in <AuthMode as ValueEnum>::value_variants() {
            let json = serde_json::to_string(variant).expect("serialize AuthMode");
            assert_eq!(json, format!("\"{}\"", variant.as_cli_str()));
            let back: AuthMode = serde_json::from_str(&json).expect("deserialize AuthMode");
            assert_eq!(back, *variant);
        }
    }

    #[test]
    fn launch_passphrase_precedence_is_explicit() {
        let cases = [
            (
                Some("rotated"),
                Some(Some("recorded")),
                Some("env"),
                Some("rotated"),
            ),
            (None, Some(Some("recorded")), Some("env"), Some("recorded")),
            // A predecessor that ran without a gate must not inherit one from
            // the restarting process.
            (None, Some(None), Some("env"), None),
            // Fresh starts and core promotion still honor the environment.
            (None, None, Some("env"), Some("env")),
            (None, None, None, None),
        ];
        for (requested, recorded, environment, expected) in cases {
            let resolved =
                resolve_launch_passphrase(requested.map(str::to_string), recorded, || {
                    environment.map(str::to_string)
                });
            assert_eq!(
                resolved.as_deref(),
                expected,
                "requested={requested:?} recorded={recorded:?} environment={environment:?}"
            );
        }
    }

    fn sample_launch() -> ServeLaunch {
        ServeLaunch {
            schema: SERVE_LAUNCH_SCHEMA,
            pid: 4242,
            instance_id: Some("instance-1".to_string()),
            profile: "work".to_string(),
            core_only: false,
            host: "0.0.0.0".to_string(),
            port: 9090,
            auth_mode: AuthMode::Passphrase,
            has_passphrase: true,
            behind_proxy: true,
            allow_plaintext_remote: true,
            read_only: true,
            cityhall: true,
            remote: false,
            tunnel_name: Some("named".to_string()),
            tunnel_url: Some("aoe.example.com".to_string()),
            no_tailscale: true,
            allowed_host: vec!["aoe.example.com".to_string()],
            allowed_origin: vec!["https://aoe.example.com:8443".to_string()],
            additional_fields: serde_json::Map::new(),
        }
    }

    #[test]
    fn tui_restart_overlay_keeps_recorded_policy() {
        use clap::Parser;
        let tui_args = |argv: &[&str]| {
            let parsed = crate::cli::Cli::try_parse_from(argv).unwrap();
            let Some(crate::cli::Commands::Serve(args)) = parsed.command else {
                unreachable!();
            };
            args
        };
        let mut recorded = sample_launch();
        recorded.auth_mode = AuthMode::Token;
        let local = recorded.overlay_tui_args(tui_args(&[
            "aoe", "serve", "--daemon", "--port", "8080", "--host", "0.0.0.0",
        ]));
        let tunnel = recorded.overlay_tui_args(tui_args(&[
            "aoe",
            "serve",
            "--daemon",
            "--port",
            "8080",
            "--remote",
            "--host",
            "127.0.0.1",
            "--no-tailscale",
        ]));
        for args in [&local, &tunnel] {
            assert!(args.read_only && args.behind_proxy && args.cityhall);
            assert!(args.dangerously_allow_plaintext_remote);
            assert_eq!(args.allowed_host, recorded.allowed_host);
            assert_eq!(args.allowed_origin, recorded.allowed_origin);
            assert_eq!(args.auth, Some(AuthMode::Token));
            assert_eq!(args.port, Some(8080));
        }
        assert!(!local.remote && local.host == "0.0.0.0");
        assert!(tunnel.remote && tunnel.host == "127.0.0.1");
        // clap requires `--remote` for the tunnel flags, so a Local restart must not replay them.
        assert_eq!((&local.tunnel_name, &local.tunnel_url), (&None, &None));
        assert_eq!(tunnel.tunnel_name, recorded.tunnel_name);
        assert_eq!(tunnel.tunnel_url, recorded.tunnel_url);

        let mut core = sample_launch();
        core.core_only = true;
        let dashboard = core.overlay_tui_args(tui_args(&[
            "aoe", "serve", "--daemon", "--port", "8080", "--host", "0.0.0.0",
        ]));
        assert!(!dashboard.core_only);
        assert_eq!((dashboard.port, dashboard.auth), (Some(8080), None));
    }

    #[test]
    fn rollback_policy_is_private_and_consumed_only_after_readiness() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir()?;
        let _app = crate::session::test_support::isolate_app_dir_at(root.path());
        let launch = launch_file("serve.rollback.launch")?;
        let secret = launch_file("serve.rollback.passphrase")?;
        launch.replace(br#"{"schema":1}"#)?;
        secret.replace(br#"{"passphrase":"secret"}"#)?;
        assert_eq!(
            std::fs::metadata(crate::session::get_app_dir()?.join("serve.rollback.passphrase"))?
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        let readiness = complete_rollback_after(Err(anyhow::anyhow!("not ready"))).unwrap_err();
        assert!(readiness.to_string().contains("not ready"));
        assert!(
            launch.read()?.is_some(),
            "failed readiness retains rollback launch"
        );
        assert!(
            secret.read()?.is_some(),
            "failed readiness retains credentials"
        );

        complete_rollback_after(Ok(()))?;
        assert!(launch.read()?.is_none());
        assert!(secret.read()?.is_none());
        assert!(complete_rollback_after(Ok(())).is_err());
        Ok(())
    }

    /// `command_is_aoe_serve` hand-parses the top-level globals to find the
    /// subcommand position in a raw command line. A global added to `Cli`
    /// without being taught there stops a real daemon's command line from
    /// parsing, which classifies it `Foreign` and deletes its lifecycle state,
    /// so derive the set from clap and fail here instead.
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
                ("no-autostart".to_string(), None),
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
            // `ps` cannot round-trip these back into argv, so the strict parse
            // fails; they must not reach the state-destroying Foreign branch.
            (&b"/Users/me/My Apps/aoe serve --daemon-child"[..], true),
            (
                &b"/usr/local/bin/aoe -p my profile serve --daemon-child"[..],
                true,
            ),
            // Clipped before `serve` (what `ps` did without `-ww`).
            (
                &b"/usr/local/bin/aoe --profile work --host 0.0.0.0 --port"[..],
                true,
            ),
            (&b"/opt/homebrew/bin/agent-of-empires serve"[..], true),
            // An unrelated process that merely mentions aoe is still foreign.
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
        // (recorded pid, recorded instance, instance is live, contradicts, authorizes)
        let cases = [
            // Recorded PID, live instance gone: the PID was recycled by another
            // aoe serve, so refuse to signal it.
            (4242, Some("instance-1"), false, true, false),
            (4242, Some("instance-1"), true, false, true),
            // A launch record for a different PID says nothing about this one, so
            // it is not a contradiction, but it authorizes nothing either.
            (99, Some("instance-1"), false, false, false),
            // No instance ID recorded (a record written before instance IDs
            // existed): the PID match is all the evidence there is, and it is
            // what pre-upgrade builds acted on.
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
    fn behind_proxy_without_allowed_host_errors_at_startup() {
        let err = validate_behind_proxy_allowlist(true, false, &[])
            .expect_err("behind-proxy with no allowed host must be rejected");
        assert!(err.to_string().contains("--allowed-host"));
    }

    #[test]
    fn behind_proxy_with_allowed_host_ok() {
        validate_behind_proxy_allowlist(true, false, &["aoe.example.com".to_string()])
            .expect("behind-proxy with an allowed host starts");
    }

    #[test]
    fn behind_proxy_remote_is_exempt_from_allowed_host() {
        validate_behind_proxy_allowlist(true, true, &[])
            .expect("remote auto-injects the tunnel host, so no flag is required");
    }

    #[test]
    fn schemeless_allowed_origin_errors_at_startup() {
        assert!(validate_allowed_origins(&["aoe.example.com:8443".to_string()]).is_err());
        assert!(validate_allowed_origins(&["".to_string()]).is_err());
    }

    #[test]
    fn hostless_allowed_origin_errors_at_startup() {
        assert!(validate_allowed_origins(&["https://".to_string()]).is_err());
        assert!(validate_allowed_origins(&["https:///".to_string()]).is_err());
        assert!(validate_allowed_origins(&["https://:8443".to_string()]).is_err());
    }

    #[test]
    fn malformed_allowed_origin_errors_at_startup() {
        assert!(validate_allowed_origins(&["https://aoe.example.com/app".to_string()]).is_err());
        assert!(validate_allowed_origins(&["https://aoe.example.com?x".to_string()]).is_err());
        assert!(validate_allowed_origins(&["https://user@aoe.example.com".to_string()]).is_err());
    }

    #[test]
    fn scheme_qualified_allowed_origin_ok() {
        validate_allowed_origins(&[
            "https://aoe.example.com:8443".to_string(),
            "http://localhost:3000".to_string(),
            "HTTPS://aoe.example.com".to_string(),
            "https://aoe.example.com/".to_string(),
            "https://[::1]".to_string(),
        ])
        .expect("full scheme://host[:port] origins (incl. IPv6, trailing slash) are accepted");
    }

    #[test]
    fn allowed_hosts_accept_bare_host_and_port() {
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
    }

    #[test]
    fn allowed_hosts_reject_untrusted_ip_literals() {
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
        ] {
            assert!(
                validate_allowed_hosts(&[host.to_string()]).is_err(),
                "{host} must be rejected as an untrusted IP literal"
            );
        }
    }

    #[test]
    fn allowed_origins_reject_untrusted_ip_literals() {
        for origin in [
            "http://0.0.0.0:8080",
            "https://[::]",
            "http://169.254.169.254",
            "https://[fe80::1]:8443",
            "http://224.0.0.1",
        ] {
            assert!(
                validate_allowed_origins(&[origin.to_string()]).is_err(),
                "{origin} must be rejected as an untrusted IP-literal origin"
            );
        }
        validate_allowed_origins(&[
            "http://127.0.0.1:3000".to_string(),
            "https://[::1]".to_string(),
            "https://192.168.1.5:8443".to_string(),
        ])
        .expect("loopback and routable IP-literal origins stay valid");
    }

    #[test]
    fn allowed_hosts_reject_malformed_authorities() {
        // pasted URL / path / query / userinfo
        assert!(validate_allowed_hosts(&["https://aoe.example.com".to_string()]).is_err());
        assert!(validate_allowed_hosts(&["aoe.example.com/app".to_string()]).is_err());
        assert!(validate_allowed_hosts(&["aoe.example.com?x".to_string()]).is_err());
        assert!(validate_allowed_hosts(&["user@aoe.example.com".to_string()]).is_err());
        // port-only: satisfies --behind-proxy's non-empty check but normalizes
        // to nothing, silently allowlisting no host (the #2735 guard defeat).
        assert!(validate_allowed_hosts(&[":8080".to_string()]).is_err());
        assert!(validate_allowed_hosts(&[":".to_string()]).is_err());
    }

    #[test]
    fn allowed_hosts_reject_empty() {
        assert!(validate_allowed_hosts(&["   ".to_string()]).is_err());
    }

    /// Parses real argv and dispatches like `main`, pinning which `serve`
    /// shapes consume `--profile` (#148).
    mod serve_state_cleanup {
        use super::super::*;
        use serial_test::serial;

        fn publish(dir: &std::path::Path, pid: u32) {
            std::fs::write(dir.join("serve.pid"), pid.to_string()).unwrap();
            for name in SERVE_STATE_FILES {
                std::fs::write(dir.join(name), format!("{name} of {pid}")).unwrap();
            }
        }

        fn assert_published(dir: &std::path::Path, pid: u32) {
            assert_eq!(
                std::fs::read_to_string(dir.join("serve.pid")).unwrap(),
                pid.to_string()
            );
            for name in SERVE_STATE_FILES {
                assert_eq!(
                    std::fs::read_to_string(dir.join(name)).unwrap(),
                    format!("{name} of {pid}")
                );
            }
        }

        #[test]
        #[serial]
        fn status_probe_leaves_the_published_state_alone() {
            let _guard = crate::session::test_support::isolate_app_dir();
            let dir = crate::session::get_app_dir().unwrap();

            let mut exited = std::process::Command::new("true").spawn().unwrap();
            exited.wait().unwrap();
            let mut foreign = std::process::Command::new("sleep")
                .arg("60")
                .spawn()
                .unwrap();

            for (name, pid) in [("exited", exited.id()), ("foreign", foreign.id())] {
                publish(&dir, pid);
                assert_eq!(daemon_status(), DaemonStatus::Absent, "{name}");
                assert_published(&dir, pid);
            }

            let _ = foreign.kill();
            let _ = foreign.wait();
        }

        #[test]
        #[serial]
        fn replacement_survives_a_probe_held_across_its_publication() {
            let _guard = crate::session::test_support::isolate_app_dir();
            let dir = crate::session::get_app_dir().unwrap();

            for (name, disposition) in [
                ("foreign", DaemonProbeDisposition::VerifyIdentity),
                ("exited", DaemonProbeDisposition::Stale),
            ] {
                publish(&dir, 4242);
                let (observed_tx, observed_rx) = std::sync::mpsc::channel();
                let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();

                let status = std::thread::scope(|scope| {
                    let old_probe = scope.spawn(move || {
                        daemon_status_with(
                            |_| {
                                observed_tx.send(()).unwrap();
                                release_rx.recv().unwrap();
                                disposition
                            },
                            |_| DaemonProcessIdentity::Foreign,
                        )
                    });
                    observed_rx.recv().unwrap();
                    {
                        let _lock = lock_serve_state().unwrap();
                        publish(&dir, 4343);
                    }
                    release_tx.send(()).unwrap();
                    old_probe.join().unwrap()
                });

                assert_eq!(status, DaemonStatus::Absent, "{name}");
                assert!(!clear_serve_state(4242), "{name}: stale cleaner must skip");
                assert_published(&dir, 4343);
                assert_eq!(
                    read_serve_urls().first().map(|u| u.url.as_str()),
                    Some("serve.url of 4343"),
                    "{name}: discovery"
                );
            }
        }

        #[test]
        #[serial]
        fn state_publication_and_cleanup_run_under_the_server_lifetime_lock() {
            let _guard = crate::session::test_support::isolate_app_dir();
            let dir = crate::session::get_app_dir().unwrap();
            let lifetime = try_lock_serve().unwrap().expect("lifetime lock is free");

            let (done_tx, done_rx) = std::sync::mpsc::channel();
            let worker = std::thread::spawn(move || {
                {
                    let _lock = lock_serve_state().unwrap();
                    publish(&dir, 4242);
                }
                let cleared = clear_serve_state(4242);
                done_tx.send(cleared).unwrap();
            });
            let cleared = done_rx
                .recv_timeout(std::time::Duration::from_secs(10))
                .expect("state lock must not wait on the lifetime lock");
            assert!(cleared);
            worker.join().unwrap();
            drop(lifetime);
        }

        #[test]
        #[serial]
        fn cleanup_deletes_only_the_state_it_inspected() {
            let _guard = crate::session::test_support::isolate_app_dir();
            let dir = crate::session::get_app_dir().unwrap();

            publish(&dir, 4242);
            assert!(!clear_serve_state(4243));
            assert_published(&dir, 4242);

            assert!(clear_serve_state(4242));
            for name in [
                "serve.pid",
                "serve.url",
                "serve.mode",
                "serve.passphrase",
                "serve.launch",
            ] {
                assert!(!dir.join(name).exists(), "{name} should be removed");
            }
        }
    }
    mod profile_guard {
        use super::super::{run, run_control};
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

        /// Isolated app dir with one profile, so the guard is armed.
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
                let (_, args) = dispatch_argv(&["aoe", "serve", verb, "-p", "ghost-profile"]);
                if let Some(Err(e)) = run_control(&args).await {
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
            // `--behind-proxy` without `--allowed-host` fails right after the
            // guard, so no port is ever bound.
            let (profile, args) =
                dispatch_argv(&["aoe", "serve", "--behind-proxy", "-p", "ghost-profile"]);
            let msg = run(&profile, args, None)
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
            let msg = run(&profile, args, None)
                .await
                .expect_err("--behind-proxy without --allowed-host is refused")
                .to_string();
            assert!(
                msg.contains("--behind-proxy requires --allowed-host"),
                "a known profile must reach the next validation, got: {msg}"
            );
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn newer_data_schema_preserves_managed_launch_and_credentials() -> Result<()> {
        let home = tempfile::tempdir()?;
        let _home = crate::session::test_support::isolate_app_dir_at(home.path());
        let app = crate::session::get_app_dir()?;
        let version = (crate::migrations::current_schema_version() + 1).to_string();
        std::fs::write(app.join(".schema_version"), &version)?;
        let original = serde_json::to_string(&serde_json::json!({
            "schema": 1,
            "pid": std::process::id(),
            "instance_id": "retained-launch",
            "auth_mode": "passphrase"
        }))?;
        for name in ["serve.launch", "serve.rollback.launch"] {
            launch_file(name)?.replace(original.as_bytes())?;
        }
        let credential = "retained-credential";
        launch_file("serve.passphrase")?.replace(credential.as_bytes())?;
        let transaction = crate::daemon::lifecycle::Transaction::acquire().await?;

        assert!(migrate_managed_launch(transaction, false).await.is_err());
        for name in ["serve.launch", "serve.rollback.launch"] {
            assert_eq!(launch_file(name)?.read()?.unwrap(), original);
        }
        assert_eq!(
            launch_file("serve.passphrase")?.read()?.unwrap(),
            credential
        );
        assert_eq!(
            std::fs::read_to_string(app.join(".schema_version"))?,
            version
        );
        Ok(())
    }
}
