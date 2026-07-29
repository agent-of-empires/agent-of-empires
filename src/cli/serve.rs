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
    fn as_cli_str(self) -> &'static str {
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

    /// Expose the dashboard over a public HTTPS tunnel. Prefers Tailscale
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
            "read_only", "passphrase", "port",
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
    /// Ignored under --daemon, --remote, SSH (SSH_CONNECTION/SSH_TTY), or when
    /// no display server is reachable on Linux/BSD.
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
            "read_only", "passphrase", "port", "host",
            "tunnel_name", "no_tailscale", "tunnel_url", "open",
            "allowed_host", "allowed_origin",
        ],
    )]
    pub restart: bool,
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
             forwards the client IP via X-Forwarded-For / cf-connecting-ip.",
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

/// Persisted launch state for a running `aoe serve --daemon`. Written
/// only by `start_daemon`, so its presence is the signal that the daemon
/// is self-managed (started by `aoe serve --daemon`) rather than run in
/// the foreground or under a service supervisor; that is what lets
/// `aoe update` decide whether it may restart the daemon. `aoe serve
/// --restart` and the post-update restart replay it. It is removed on
/// stop, on the daemon's own graceful exit, and in `daemon_pid`'s
/// stale-PID sweep, so nothing relies on a stale copy. The passphrase is
/// never stored here; it is recalled from `serve.passphrase` /
/// `AOE_SERVE_PASSPHRASE` at restart time.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ServeLaunch {
    pub schema: u32,
    pub pid: u32,
    pub profile: String,
    pub host: String,
    pub port: u16,
    pub auth_mode: AuthMode,
    pub behind_proxy: bool,
    pub read_only: bool,
    pub remote: bool,
    pub tunnel_name: Option<String>,
    pub tunnel_url: Option<String>,
    pub no_tailscale: bool,
    #[serde(default)]
    pub allowed_host: Vec<String>,
    #[serde(default)]
    pub allowed_origin: Vec<String>,
}

const SERVE_LAUNCH_SCHEMA: u32 = 1;

impl ServeLaunch {
    /// Rebuild the `ServeArgs` needed to relaunch this daemon. The
    /// effective auth mode is replayed via `--auth`; clap's `--no-auth`
    /// alias is not needed since `--auth=none` covers it. The passphrase
    /// is injected by the caller after recall.
    fn to_serve_args(&self, passphrase: Option<String>) -> ServeArgs {
        ServeArgs {
            port: Some(self.port),
            host: self.host.clone(),
            auth: Some(self.auth_mode),
            no_auth: false,
            behind_proxy: self.behind_proxy,
            read_only: self.read_only,
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

/// True when relaunching this config requires a passphrase that must be
/// recovered before the running daemon is stopped: remote mode mandates
/// one, and `--auth=passphrase` is meaningless without it.
fn launch_needs_passphrase(launch: &ServeLaunch) -> bool {
    launch.remote || matches!(launch.auth_mode, AuthMode::Passphrase)
}

fn serve_launch_path() -> Result<PathBuf> {
    let dir = crate::session::get_app_dir()?;
    Ok(dir.join("serve.launch"))
}

/// True when a `serve.launch` file exists, i.e. a daemon started by
/// `aoe serve --daemon` recorded its launch state. `aoe update` uses this
/// to decide whether a running daemon is one it may restart.
pub fn serve_launch_exists() -> bool {
    serve_launch_path().map(|p| p.exists()).unwrap_or(false)
}

/// Write `serve.launch` with owner-only (0600) permissions: it records
/// the daemon's bind host/port, tunnel URL, auth posture, and profile,
/// which should not be world-readable on a shared machine.
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

/// Recall the daemon passphrase for a restart: the plaintext
/// `serve.passphrase` file the server writes while running first,
/// then the `AOE_SERVE_PASSPHRASE` env override. Returns None when
/// neither yields a non-empty value.
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

/// Cached read of `$APP_DIR/serve.mode`, keyed on the current daemon
/// PID. The status bar calls this on every render frame; without
/// caching, that's a syscall + file read per frame just to compute a
/// one-word label. We re-read the mode file only when the PID changes
/// (daemon restart, fresh spawn), which is exactly when the mode could
/// have changed.
///
/// Returns `None` when no daemon is running OR when the mode file is
/// missing/unparseable. Callers can treat both cases the same way:
/// "show the generic Serving label, no mode tag."
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
        // Lock poisoned (only happens if a previous holder panicked
        // while reading the file); fall back to a fresh read so the
        // status bar still works.
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

/// Cross-platform check that `pid` belongs to an aoe / agent-of-empires
/// process. PIDs get recycled, so `kill(pid, 0) == Ok` is not enough on
/// its own — we also want to know it's actually *our* daemon.
///
/// Returns `true` if the process looks like ours, `false` otherwise.
/// If we can't determine either way (platform lacks the lookup, ps
/// missing), we return `true` so behavior matches the legacy Linux path
/// of trusting the PID file rather than falsely flagging a real daemon
/// as foreign.
fn verify_pid_is_aoe(pid: i32) -> bool {
    // Linux fast path: read /proc directly, no subprocess.
    let proc_path = format!("/proc/{}/cmdline", pid);
    if std::path::Path::new(&proc_path).exists() {
        if let Ok(cmdline) = std::fs::read_to_string(&proc_path) {
            return cmdline.contains("aoe") || cmdline.contains("agent-of-empires");
        }
    }

    // macOS / other: shell out to `ps`. `-o command=` prints the full
    // command (path + args) with no header.
    match std::process::Command::new("ps")
        .args(["-o", "command=", "-p", &pid.to_string()])
        .output()
    {
        Ok(out) if out.status.success() => {
            let s = String::from_utf8_lossy(&out.stdout);
            s.contains("aoe") || s.contains("agent-of-empires")
        }
        // ps failed or unavailable — we can't verify, so trust the PID
        // file rather than ghosting a real daemon.
        _ => true,
    }
}

/// Returns Some(pid) if the daemon's PID file exists AND the process is
/// still alive AND it looks like one of our aoe processes. Cleans up
/// stale PID files it finds. The TUI uses this both to jump straight to
/// the Active state when the Remote Access dialog opens and to render
/// the "● Remote on" status-bar indicator.
pub fn daemon_pid() -> Option<u32> {
    let path = pid_file_path().ok()?;
    let pid_str = std::fs::read_to_string(&path).ok()?;
    let pid: i32 = pid_str.trim().parse().ok()?;

    match nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None) {
        Ok(()) => {
            if verify_pid_is_aoe(pid) {
                Some(pid as u32)
            } else {
                // PID was recycled by an unrelated process — our daemon
                // is dead. Clean up the stale file so subsequent callers
                // don't keep false-positive-ing.
                let _ = std::fs::remove_file(&path);
                if let Ok(dir) = crate::session::get_app_dir() {
                    let _ = std::fs::remove_file(dir.join("serve.url"));
                    let _ = std::fs::remove_file(dir.join("serve.mode"));
                    let _ = std::fs::remove_file(dir.join("serve.passphrase"));
                    let _ = std::fs::remove_file(dir.join("serve.launch"));
                }
                None
            }
        }
        Err(_) => {
            // Stale PID file; the ESRCH case is handled the same as any
            // other error — the process is not reachable.
            let _ = std::fs::remove_file(&path);
            if let Ok(dir) = crate::session::get_app_dir() {
                let _ = std::fs::remove_file(dir.join("serve.url"));
                let _ = std::fs::remove_file(dir.join("serve.mode"));
                let _ = std::fs::remove_file(dir.join("serve.passphrase"));
                let _ = std::fs::remove_file(dir.join("serve.launch"));
            }
            None
        }
    }
}

#[tracing::instrument(target = "cli.serve", skip_all, fields(profile = %profile))]
pub async fn run(profile: &str, args: ServeArgs) -> Result<()> {
    if args.stop {
        return stop_daemon().await;
    }

    if args.status {
        return print_status().await;
    }

    if args.restart {
        return restart_daemon().await;
    }

    // A fresh start with `aoe.web` disabled never reaches here: `main` rejects
    // it as an unrecognized subcommand before dispatch (the lifecycle verbs
    // above are exempt so a running daemon can always be brought down).

    // Refuse to start a second instance (daemon or foreground) while another
    // aoe serve is already running. Without this gate, a foreground
    // `aoe serve` would overwrite the existing daemon's PID file in the
    // non-daemon write below before its own port-bind eventually failed; the
    // post-exit cleanup would then delete the (now-foreground) PID file and
    // orphan the real daemon.
    //
    // Skip the bail if the PID file already points to our own process: that
    // means we are the daemonized child that start_daemon() just spawned and
    // pre-populated the file for, not a competing instance.
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

    // --behind-proxy + --remote is meaningless: --remote manages its
    // own ingress, --behind-proxy assumes an external one. Warn but
    // do not hard-fail; --remote wins for the tunnel-spawn decision
    // and both set behind_tunnel anyway. Emit on both stderr (for
    // foreground users) and the tracing pipeline (for daemon users
    // whose stderr lands inside debug.log unread).
    if args.behind_proxy && args.remote {
        let msg = "--behind-proxy is ignored when --remote is set; \
             --remote already enables the equivalent cookie-Secure and \
             trusted-XFF behavior and manages its own ingress.";
        eprintln!("Note: {msg}");
        tracing::warn!(target: "serve", "{msg}");
    }

    // Named tunnel requires --tunnel-url
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

    // Remote mode: check cloudflared (only when Tailscale Funnel can't carry the
    // traffic) and force localhost binding. start_server() prefers Tailscale when
    // it's available, so requiring cloudflared up front would falsely reject
    // Tailscale-only setups (issue #813).
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
        // Force localhost since the tunnel connects to localhost
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

    // Block remote mode without passphrase
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
        let _ = tokio::fs::write(&path, std::process::id().to_string()).await;
        tracing::debug!(target: "serve.lifecycle", path = %path.display(), pid = std::process::id(), "wrote pid file");
    }

    let result = crate::server::start_server(crate::server::ServerConfig {
        profile,
        host: &host,
        port: args.resolved_port(),
        no_auth: matches!(auth_mode, AuthMode::Passphrase | AuthMode::None),
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

    // Clean up PID and URL files on exit, but only if the PID file
    // still belongs to this process. A newer daemon spawn may have
    // overwritten it; removing their file would orphan them.
    if let Ok(path) = pid_file_path() {
        let is_ours = tokio::fs::read_to_string(&path)
            .await
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .is_some_and(|pid| pid == std::process::id());
        if is_ours {
            let _ = tokio::fs::remove_file(&path).await;
            if let Ok(dir) = crate::session::get_app_dir() {
                let _ = tokio::fs::remove_file(dir.join("serve.url")).await;
                let _ = tokio::fs::remove_file(dir.join("serve.mode")).await;
                let _ = tokio::fs::remove_file(dir.join("serve.passphrase")).await;
                let _ = tokio::fs::remove_file(dir.join("serve.launch")).await;
            }
        }
    }

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

fn start_daemon(profile: &str, args: &ServeArgs) -> Result<()> {
    use std::process::{Command, Stdio};

    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
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

    cmd.stdin(Stdio::null());

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
    // a rotation; that is best-effort behavior documented in
    // docs/development/logging.md.
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

    // Write PID file
    if let Ok(path) = pid_file_path() {
        std::fs::write(&path, pid.to_string())?;
        tracing::debug!(target: "serve.lifecycle", path = %path.display(), pid, "wrote pid file");
    }

    // Persist the launch state so `aoe serve --restart` and the
    // post-update restart can replay this daemon's exact config. Mirrors
    // the argv reconstruction above; the passphrase is deliberately left
    // out (recalled from serve.passphrase / the env on restart).
    let launch = ServeLaunch {
        schema: SERVE_LAUNCH_SCHEMA,
        pid,
        profile: profile.to_string(),
        host: args.host.clone(),
        port: args.resolved_port(),
        auth_mode: resolve_auth_mode(args.auth, args.no_auth),
        behind_proxy: args.behind_proxy,
        read_only: args.read_only,
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

/// Restart the running `aoe serve` daemon from its persisted launch state
/// (`serve.launch`). Everything needed to relaunch is read into memory
/// before the old daemon is stopped, because `stop_daemon` deletes
/// `serve.passphrase`; a daemon that needs a passphrase is never killed
/// without the means to bring it back. The replacement is spawned via
/// `start_daemon`, which uses the current executable. That is correct
/// both for a hand-run `aoe serve --restart` and for the post-update
/// path, where `aoe update` re-execs the freshly installed binary as
/// `aoe serve --restart` so the new code spawns the new daemon.
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

    // Recall the passphrase BEFORE stopping: stop_daemon() deletes
    // serve.passphrase, so reading it afterwards would always fail.
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

    // Re-validate the persisted config before tearing anything down, so a
    // config that can no longer start (e.g. policy changed) fails loudly
    // instead of leaving the user with no daemon.
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
    let pid: i32 = pid_str
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("Invalid PID in {}: {}", path.display(), pid_str.trim()))?;
    tracing::info!(target: "serve.shutdown", pid, "sending SIGTERM to daemon");

    // Verify PID belongs to an aoe process on all platforms
    if !verify_pid_is_aoe(pid) {
        tokio::fs::remove_file(&path).await?;
        bail!(
            "PID {} belongs to a different process (stale PID file). Cleaned up.",
            pid
        );
    }

    // Send SIGTERM
    match nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid),
        nix::sys::signal::Signal::SIGTERM,
    ) {
        Ok(()) => {
            // Wait for the process to actually exit so the port is
            // released before a new daemon can be spawned. Without
            // this, closing the dialog and immediately reopening
            // races with the dying daemon and can orphan it.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                match nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None) {
                    Err(nix::errno::Errno::ESRCH) => break,
                    _ if std::time::Instant::now() >= deadline => {
                        // Still alive after timeout; escalate.
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
            // The daemon's own cleanup may have already removed some
            // of these; that's fine.
            let _ = tokio::fs::remove_file(&path).await;
            if let Ok(dir) = crate::session::get_app_dir() {
                let _ = tokio::fs::remove_file(dir.join("serve.url")).await;
                let _ = tokio::fs::remove_file(dir.join("serve.mode")).await;
                let _ = tokio::fs::remove_file(dir.join("serve.passphrase")).await;
                let _ = tokio::fs::remove_file(dir.join("serve.launch")).await;
            }
            println!("Stopped aoe serve daemon (PID {})", pid);
        }
        Err(nix::errno::Errno::ESRCH) => {
            // Process doesn't exist; clean up stale PID file
            tokio::fs::remove_file(&path).await?;
            if let Ok(dir) = crate::session::get_app_dir() {
                let _ = tokio::fs::remove_file(dir.join("serve.url")).await;
                let _ = tokio::fs::remove_file(dir.join("serve.mode")).await;
                let _ = tokio::fs::remove_file(dir.join("serve.passphrase")).await;
                let _ = tokio::fs::remove_file(dir.join("serve.launch")).await;
            }
            println!("Daemon was not running (stale PID file cleaned up)");
        }
        Err(e) => bail!("Failed to stop daemon (PID {}): {}", pid, e),
    }

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

#[cfg(test)]
mod tests {
    use super::*;

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

    fn sample_launch() -> ServeLaunch {
        ServeLaunch {
            schema: SERVE_LAUNCH_SCHEMA,
            pid: 4242,
            profile: "work".to_string(),
            host: "0.0.0.0".to_string(),
            port: 9090,
            auth_mode: AuthMode::Passphrase,
            behind_proxy: true,
            read_only: true,
            remote: false,
            tunnel_name: Some("named".to_string()),
            tunnel_url: Some("aoe.example.com".to_string()),
            no_tailscale: true,
            allowed_host: vec!["aoe.example.com".to_string()],
            allowed_origin: vec!["https://aoe.example.com:8443".to_string()],
        }
    }

    #[test]
    fn serve_launch_json_round_trips() {
        let launch = sample_launch();
        let json = serde_json::to_string(&launch).expect("serialize");
        let back: ServeLaunch = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.pid, launch.pid);
        assert_eq!(back.profile, launch.profile);
        assert_eq!(back.host, launch.host);
        assert_eq!(back.port, launch.port);
        assert_eq!(back.auth_mode, launch.auth_mode);
        assert_eq!(back.behind_proxy, launch.behind_proxy);
        assert_eq!(back.read_only, launch.read_only);
        assert_eq!(back.remote, launch.remote);
        assert_eq!(back.tunnel_name, launch.tunnel_name);
        assert_eq!(back.tunnel_url, launch.tunnel_url);
        assert_eq!(back.no_tailscale, launch.no_tailscale);
        assert_eq!(back.allowed_host, launch.allowed_host);
        assert_eq!(back.allowed_origin, launch.allowed_origin);
    }

    #[test]
    fn to_serve_args_replays_launch_config() {
        let launch = sample_launch();
        let args = launch.to_serve_args(Some("hunter2".to_string()));
        // Bind config is replayed; auth goes through --auth, never the
        // --no-auth alias; daemon mode is forced; the child markers and
        // restart flag are cleared so re-entry does not loop.
        assert_eq!(args.port, Some(9090));
        assert_eq!(args.host, "0.0.0.0");
        assert_eq!(args.auth, Some(AuthMode::Passphrase));
        assert!(!args.no_auth);
        assert!(args.behind_proxy);
        assert!(args.read_only);
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
}
