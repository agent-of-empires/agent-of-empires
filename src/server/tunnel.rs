//! Public-HTTPS tunnel integration for secure remote access.

use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

/// Manages a public-HTTPS tunnel.
pub struct TunnelHandle {
    /// None for Tailscale (no child process to supervise).
    child: Option<Arc<Mutex<Child>>>,
    pub url: String,
    port: u16,
    kind: TunnelKind,
    cancel: CancellationToken,
}

#[derive(Clone)]
enum TunnelKind {
    Quick,
    Named { tunnel_name: String },
    Tailscale,
}

impl TunnelKind {
    /// Cloudflare tunnels set `cf-connecting-ip`; Tailscale Funnel rewrites `x-forwarded-for`
    /// but passes a caller's `cf-connecting-ip` through.
    fn sets_cf_connecting_ip(&self) -> bool {
        !matches!(self, Self::Tailscale)
    }

    /// Short label for `serve.mode` and the TUI status bar.
    pub fn mode_label(&self) -> &'static str {
        match self {
            Self::Quick | Self::Named { .. } => "tunnel",
            Self::Tailscale => "tailscale",
        }
    }
}

impl TunnelHandle {
    pub fn sets_cf_connecting_ip(&self) -> bool {
        self.kind.sets_cf_connecting_ip()
    }

    pub fn mode_label(&self) -> &'static str {
        self.kind.mode_label()
    }

    pub fn is_stable_origin(&self) -> bool {
        // Quick CF tunnels rotate; named CF and Tailscale are stable.
        !matches!(self.kind, TunnelKind::Quick)
    }
}

impl TunnelHandle {
    /// Spawn a quick tunnel (zero-config, random subdomain, no account needed).
    pub async fn spawn_quick(local_port: u16) -> anyhow::Result<Self> {
        let mut child = Command::new("cloudflared")
            .args([
                "tunnel",
                "--url",
                &format!("http://localhost:{}", local_port),
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| {
                anyhow::anyhow!(
                    "Failed to start cloudflared: {}.\n\
                     Install it: https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/downloads/",
                    e
                )
            })?;

        let stderr = child.stderr.take().expect("stderr was piped");
        let mut reader = BufReader::new(stderr).lines();

        let url = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            while let Some(line) = reader.next_line().await? {
                if let Some(url) = extract_tunnel_url(&line) {
                    return Ok::<String, anyhow::Error>(url);
                }
            }
            anyhow::bail!("cloudflared exited without providing a tunnel URL")
        })
        .await
        .map_err(|_| anyhow::anyhow!("Timed out waiting for cloudflared tunnel URL (30s)"))??;

        // Drain remaining stderr to prevent pipe buffer deadlock
        tokio::spawn(async move { while let Ok(Some(_)) = reader.next_line().await {} });

        info!(url = %url, "Cloudflare tunnel established");

        Ok(TunnelHandle {
            child: Some(Arc::new(Mutex::new(child))),
            url,
            port: local_port,
            kind: TunnelKind::Quick,
            cancel: CancellationToken::new(),
        })
    }

    /// Spawn a named tunnel (requires prior `cloudflared tunnel create` and DNS setup).
    pub async fn spawn_named(
        tunnel_name: &str,
        tunnel_url: &str,
        local_port: u16,
    ) -> anyhow::Result<Self> {
        let child = Command::new("cloudflared")
            .args([
                "tunnel",
                "run",
                "--url",
                &format!("http://localhost:{}", local_port),
                tunnel_name,
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| {
                anyhow::anyhow!(
                    "Failed to start named tunnel '{}': {}.\n\
                     Make sure you have run `cloudflared tunnel create {}` first.",
                    tunnel_name,
                    e,
                    tunnel_name
                )
            })?;

        // Give cloudflared a moment to connect
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;

        let domain = tunnel_url
            .trim_start_matches("https://")
            .trim_start_matches("http://");
        if domain.is_empty()
            || domain.contains(' ')
            || domain.contains('/')
            || !domain.contains('.')
        {
            return Err(anyhow::anyhow!(
                "Invalid tunnel URL '{}'. Expected a domain like 'aoe.example.com'.",
                tunnel_url
            ));
        }

        let url = format!("https://{}", domain);

        info!(url = %url, tunnel = %tunnel_name, "Named Cloudflare tunnel started");

        Ok(TunnelHandle {
            child: Some(Arc::new(Mutex::new(child))),
            url,
            port: local_port,
            kind: TunnelKind::Named {
                tunnel_name: tunnel_name.to_string(),
            },
            cancel: CancellationToken::new(),
        })
    }

    /// Configure Tailscale Funnel for the local port and return a handle carrying the
    /// stable `https://<host>.<tailnet>.ts.net` URL.
    pub async fn spawn_tailscale(local_port: u16) -> anyhow::Result<Self> {
        use anyhow::Context as _;
        let home =
            dirs::home_dir().context("couldn't find the home directory for the Funnel lock")?;
        let lock = funnel_lock_path(&home);
        Self::spawn_tailscale_with(std::ffi::OsStr::new("tailscale"), &lock, local_port).await
    }

    async fn spawn_tailscale_with(
        program: &std::ffi::OsStr,
        lock: &std::path::Path,
        local_port: u16,
    ) -> anyhow::Result<Self> {
        // Hard cap on each tailscale command so we never wedge if tailscale pops an
        // interactive prompt (HTTPS-certs consent, Funnel-not-enabled-in-ACL, node not
        // signed in).
        const STEP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
        debug!(
            local_port = local_port,
            timeout_s = STEP_TIMEOUT.as_secs(),
            "tailscale: spawn_tailscale starting"
        );

        // Pre-flight 1.
        if let Err(e) = check_funnel_capability(program).await {
            debug!(reason = %e, "spawn_tailscale: funnel cap check failed");
            anyhow::bail!(
                "Tailscale Funnel is not enabled for this tailnet. \
                 Enable it at https://login.tailscale.com/admin/acls/file \
                 (add `funnel` to nodeAttrs), or pass --no-tailscale to use Cloudflare."
            );
        }

        // Held until this start succeeds or its reset ends; the guard owns it, so it's released
        // after the guard's own `off`. Another aoe daemon can't activate in between.
        let funnel_lock = lock_funnel_starts(lock).await?;

        // Pre-flight 2.
        let preflight = inspect_existing_funnel(program, local_port).await;
        if let FunnelPreflight::Refuse(why) = &preflight {
            anyhow::bail!("{why}");
        }

        // Single-command Funnel (Tailscale 1.52+).
        let funnel_arg = local_port.to_string();
        let funnel_args = ["funnel", "--bg", "--yes", &funnel_arg];
        info!(
            "Running `tailscale funnel --bg --yes {}` (first-time HTTPS cert provisioning can take 30-60s)",
            local_port
        );

        // If :443 had nothing on it before, everything there from now on is this start's: a
        // start that fails or is cancelled turns it back off, even while `--bg` is still running
        // (it may already have applied the config), so no Funnel is left pointing at a port
        // nothing serves. A public Funnel that was already there (the user's own, or a sibling
        // path) keeps its other routes; only `/` changes.
        let mut guard = FunnelStartGuard::new(
            program,
            local_port,
            preflight == FunnelPreflight::Unused,
            funnel_lock,
        );

        let child = Command::new(program)
            .args(funnel_args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn();
        let mut child = match child {
            Ok(child) => child,
            Err(e) => {
                // `--bg` never ran, so there's nothing to turn off.
                guard.disarm();
                anyhow::bail!("failed to run tailscale funnel: {e}");
            }
        };
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        // Spawn drain tasks for both streams.
        let (activation_tx, mut activation_rx) = tokio::sync::oneshot::channel::<String>();
        let activation_tx = std::sync::Arc::new(std::sync::Mutex::new(Some(activation_tx)));
        if let Some(stderr) = stderr {
            let activation_tx = activation_tx.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    info!(source = "tailscale funnel", "{}", line);
                    if let Some(url) = extract_funnel_activation_url(&line) {
                        if let Some(tx) = activation_tx.lock().ok().and_then(|mut g| g.take()) {
                            let _ = tx.send(url);
                        }
                    }
                }
            });
        }
        if let Some(stdout) = stdout {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    debug!(source = "tailscale funnel stdout", "{}", line);
                }
            });
        }

        let status = tokio::select! {
            biased;
            maybe_url = &mut activation_rx => {
                // Tailscale told us exactly how to fix this.
                let _ = child.kill().await;
                let detail = match maybe_url {
                    Ok(url) => format!(
                        "Enable it here (the link is specific to this node):\n  {url}"
                    ),
                    Err(_) => "Enable it from your tailnet admin console under \
                              Settings > Features > Funnel.".to_string(),
                };
                let e = anyhow::anyhow!(
                    "Tailscale Funnel is not enabled for this tailnet.\n\n\
                     {detail}\n\n\
                     After enabling, re-run `aoe serve --remote`. \
                     Or pass --no-tailscale to use Cloudflare instead."
                );
                return Err(guard.fail(e).await);
            }
            // `--bg` may have applied the config before it failed, so these exits go through
            // `fail()` too, and the error says what was left on.
            res = tokio::time::timeout(STEP_TIMEOUT, child.wait()) => match res {
                Ok(Ok(status)) => status,
                Ok(Err(e)) => {
                    let _ = child.kill().await;
                    let e = anyhow::anyhow!("failed to wait on tailscale funnel: {e}");
                    return Err(guard.fail(e).await);
                }
                Err(_) => {
                    // Stop it first, so it can't change :443 while the reset reads it.
                    let _ = child.kill().await;
                    let e = anyhow::anyhow!(
                        "tailscale funnel timed out after {}s; your node may not have \
                         HTTPS certs enabled or Funnel may not be enabled in your tailnet \
                         ACL. Run `AGENT_OF_EMPIRES_DEBUG=1 aoe serve --remote` and \
                         check debug.log for the live output, or try \
                         `tailscale funnel --bg --yes {}` manually, or pass \
                         --no-tailscale to skip.",
                        STEP_TIMEOUT.as_secs(),
                        local_port
                    );
                    return Err(guard.fail(e).await);
                }
            },
        };

        info!(
            "`tailscale funnel` exited with status {:?} ({})",
            status.code(),
            if status.success() { "ok" } else { "error" }
        );
        if !status.success() {
            let e = anyhow::anyhow!(
                "tailscale funnel exited with status {:?}. Confirm Funnel is enabled \
                 in your tailnet ACL (https://login.tailscale.com/admin/acls/file) \
                 and HTTPS is enabled on the node. Run with \
                 AGENT_OF_EMPIRES_DEBUG=1 to see the full stderr stream.",
                status.code()
            );
            return Err(guard.fail(e).await);
        }

        // Read the stable funnel URL from `tailscale status`.
        info!("Reading stable URL from `tailscale status --json`...");
        let url = match tokio::time::timeout(STEP_TIMEOUT, tailscale_funnel_url(program)).await {
            Ok(Ok(url)) => url,
            Ok(Err(e)) => return Err(guard.fail(e).await),
            Err(_) => {
                let e = anyhow::anyhow!(
                    "tailscale status timed out after {}s",
                    STEP_TIMEOUT.as_secs()
                );
                return Err(guard.fail(e).await);
            }
        };
        guard.disarm();
        info!(url = %url, "Tailscale Funnel established");

        Ok(TunnelHandle {
            child: None,
            url,
            port: local_port,
            kind: TunnelKind::Tailscale,
            cancel: CancellationToken::new(),
        })
    }

    /// Gracefully shut down the tunnel process.
    #[tracing::instrument(target = "serve.tunnel", skip_all)]
    pub async fn shutdown(self) {
        self.cancel.cancel();
        // Brief yield to let the monitor task observe cancellation
        tokio::task::yield_now().await;

        let Some(child_arc) = self.child else {
            tracing::info!(target: "serve.tunnel", "Tailscale Funnel handle released (Funnel config left in place)");
            return;
        };
        let mut child = child_arc.lock().await;
        if let Some(id) = child.id() {
            tracing::info!(target: "serve.tunnel", pid = id, "sending SIGTERM to cloudflared");
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(id as i32),
                nix::sys::signal::Signal::SIGTERM,
            );
        }
        match tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await {
            Ok(_) => tracing::info!(target: "serve.tunnel", "Cloudflare tunnel stopped cleanly"),
            Err(_) => {
                tracing::warn!(target: "serve.tunnel", "Cloudflare tunnel did not stop in 5s, killing");
                let _ = child.kill().await;
            }
        }
    }

    /// Spawn a background task that monitors tunnel health and attempts one restart.
    pub fn spawn_health_monitor(&self) {
        let Some(child_arc) = self.child.as_ref() else {
            return; // Tailscale: no child to monitor
        };
        let child = Arc::clone(child_arc);
        let kind = self.kind.clone();
        let port = self.port;
        let cancel = self.cancel.clone();

        tokio::spawn(async move {
            let mut has_restarted = false;
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = tokio::time::sleep(std::time::Duration::from_secs(10)) => {}
                }

                // Read try_wait under the lock, then drop the guard before any await.
                let exit_status = {
                    let mut child_guard = child.lock().await;
                    match child_guard.try_wait() {
                        Ok(Some(status)) => Some(status),
                        Ok(None) => None,
                        Err(e) => {
                            warn!("Error checking tunnel status: {}", e);
                            None
                        }
                    }
                };

                let Some(status) = exit_status else {
                    continue;
                };

                if has_restarted {
                    error!(
                        "Cloudflare tunnel exited again ({}). \
                         Remote access is unavailable. \
                         Restart with `aoe serve --remote`.",
                        status
                    );
                    return;
                }

                warn!(
                    "Cloudflare tunnel exited unexpectedly ({}). Attempting restart...",
                    status
                );

                let restart_result = tokio::select! {
                    _ = cancel.cancelled() => return,
                    r = restart_tunnel(&kind, port) => r,
                };

                match restart_result {
                    Ok(new_child) => {
                        // Re-acquire the lock just long enough to install the replacement
                        // child.
                        *child.lock().await = new_child;
                        has_restarted = true;
                        info!("Cloudflare tunnel restarted successfully");
                    }
                    Err(e) => {
                        error!(
                            "Failed to restart tunnel: {}. \
                             Remote access is unavailable.",
                            e
                        );
                        return;
                    }
                }
            }
        });
    }
}

async fn restart_tunnel(kind: &TunnelKind, port: u16) -> anyhow::Result<Child> {
    match kind {
        TunnelKind::Quick => {
            let child = Command::new("cloudflared")
                .args(["tunnel", "--url", &format!("http://localhost:{}", port)])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true)
                .spawn()?;
            Ok(child)
        }
        TunnelKind::Named { tunnel_name } => {
            let child = Command::new("cloudflared")
                .args([
                    "tunnel",
                    "run",
                    "--url",
                    &format!("http://localhost:{}", port),
                    tunnel_name,
                ])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true)
                .spawn()?;
            Ok(child)
        }
        TunnelKind::Tailscale => {
            // Unreachable in practice.
            anyhow::bail!("restart_tunnel called for Tailscale; no child process exists")
        }
    }
}

/// What probing the local `tailscale` found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TailscaleProbe {
    /// On PATH, and the daemon is running and signed in.
    Available,
    /// Missing, or it answered that it isn't usable.
    Unavailable,
    /// A probe didn't answer in time, so it may still be a healthy but slow Tailscale.
    TimedOut,
}

impl TailscaleProbe {
    /// Whether to start Tailscale Funnel. A timeout fails closed rather than silently picking a
    /// Cloudflare quick tunnel, whose rotating URL breaks installed PWAs.
    pub fn use_funnel(self) -> anyhow::Result<bool> {
        match self {
            Self::Available => Ok(true),
            Self::Unavailable => Ok(false),
            Self::TimedOut => anyhow::bail!(
                "`tailscale` didn't answer within {}s, so aoe couldn't tell whether to use \
                 Tailscale Funnel. Re-run `aoe serve --remote` once Tailscale responds, or pass \
                 `--no-tailscale` to use Cloudflare intentionally (quick-tunnel URL rotates on \
                 restart).",
                PROBE_TIMEOUT.as_secs()
            ),
        }
    }
}

/// Probe whether `tailscale` is on PATH and its daemon is running and signed in.
pub async fn tailscale_probe() -> TailscaleProbe {
    tailscale_probe_with(std::ffi::OsStr::new("tailscale"), PROBE_TIMEOUT).await
}

async fn tailscale_probe_with(
    program: &std::ffi::OsStr,
    timeout: std::time::Duration,
) -> TailscaleProbe {
    // Each probe is bounded so a wedged `tailscale` falls back to Cloudflare at once instead of
    // holding tunnel selection until the startup deadline; dropping one kills its child.
    let probe = |arg: &'static str| async move {
        let output = Command::new(program).arg(arg).kill_on_drop(true).output();
        match tokio::time::timeout(timeout, output).await {
            Ok(result) => result,
            Err(_) => Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!(
                    "`tailscale {arg}` timed out after {:.1}s",
                    timeout.as_secs_f32()
                ),
            )),
        }
    };
    // Cheapest possible check first.
    let version = probe("--version").await;
    match &version {
        Ok(o) => debug!(
            exit = ?o.status.code(),
            success = o.status.success(),
            stdout = %String::from_utf8_lossy(&o.stdout).trim(),
            stderr = %String::from_utf8_lossy(&o.stderr).trim(),
            "tailscale_available: `tailscale --version` returned"
        ),
        Err(e) => debug!(error = %e, "tailscale_available: `tailscale --version` spawn failed"),
    }
    let out = match version {
        Ok(out) => out,
        Err(e) if e.kind() == std::io::ErrorKind::TimedOut => return TailscaleProbe::TimedOut,
        Err(_) => return TailscaleProbe::Unavailable,
    };
    if !out.status.success() {
        return TailscaleProbe::Unavailable;
    }
    // Then confirm the daemon is running and signed in.
    let status = probe("status").await;
    match &status {
        Ok(o) => debug!(
            exit = ?o.status.code(),
            success = o.status.success(),
            stderr = %String::from_utf8_lossy(&o.stderr).trim(),
            "tailscale_available: `tailscale status` returned"
        ),
        Err(e) => debug!(error = %e, "tailscale_available: `tailscale status` spawn failed"),
    }
    match status {
        Ok(out) if out.status.success() => TailscaleProbe::Available,
        Err(e) if e.kind() == std::io::ErrorKind::TimedOut => TailscaleProbe::TimedOut,
        _ => TailscaleProbe::Unavailable,
    }
}

/// The `tailscale funnel` arguments that turn off the :443 Funnel. Without `--set-path` this
/// removes every path on :443, so it only runs when the pre-flight found :443 unused;
/// `tailscale funnel reset` would clear every port.
const FUNNEL_OFF_ARGS: [&str; 3] = ["funnel", "--https=443", "off"];
/// The lock that serializes Tailscale starts across aoe daemons. It sits in the home directory, not
/// beside an app dir: a release and a debug build may resolve different app-dir layouts (legacy
/// `~/.agent-of-empires` next to an XDG `agent-of-empires-dev`), and must still share it.
fn funnel_lock_path(home: &std::path::Path) -> std::path::PathBuf {
    home.join(".agent-of-empires-tailscale-funnel.lock")
}

/// How long to wait for another aoe daemon's Tailscale start. Below the 90 s deadline on the whole
/// tunnel start, so this error (with its `--no-tailscale` hint) is the one the user sees.
const FUNNEL_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

/// Lock paths a start has found held by another, so a test can wait until one is really blocked.
#[cfg(test)]
static CONTENDED_FUNNEL_LOCKS: std::sync::Mutex<Vec<std::path::PathBuf>> =
    std::sync::Mutex::new(Vec::new());

/// Wait, bounded and cancellable, for the cross-daemon Funnel lock.
async fn lock_funnel_starts(path: &std::path::Path) -> anyhow::Result<std::fs::File> {
    use anyhow::Context as _;
    use fs2::FileExt as _;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .with_context(|| {
            format!(
                "couldn't open {}, which aoe locks while starting Tailscale Funnel; check that the \
                 home directory is writable, or pass --no-tailscale to use Cloudflare.",
                path.display()
            )
        })?;
    let deadline = tokio::time::Instant::now() + FUNNEL_LOCK_WAIT;
    let mut logged = false;
    loop {
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(file),
            Err(e) if e.kind() == fs2::lock_contended_error().kind() => {
                if tokio::time::Instant::now() >= deadline {
                    anyhow::bail!(
                        "another aoe daemon has been starting Tailscale Funnel for over {}s; \
                         try again once it finishes, or pass --no-tailscale to use Cloudflare.",
                        FUNNEL_LOCK_WAIT.as_secs()
                    );
                }
                if !std::mem::replace(&mut logged, true) {
                    info!(
                        target: "serve.tunnel",
                        "Waiting for another aoe daemon's Tailscale Funnel start to finish"
                    );
                    #[cfg(test)]
                    CONTENDED_FUNNEL_LOCKS
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(path.to_owned());
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            Err(e) => return Err(e).with_context(|| format!("couldn't lock {}", path.display())),
        }
    }
}

/// How long turning the Funnel back off may take.
const FUNNEL_OFF_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// Turns off the :443 Funnel a Tailscale start switched on, unless the start finishes.
/// Every error return after `--bg` starts awaits [`Self::fail`]; `Drop` covers a start that was
/// cancelled, and any other early return.
struct FunnelStartGuard {
    program: std::ffi::OsString,
    port: u16,
    armed: bool,
    /// The cross-daemon Funnel lock; a field, so it's released only after `Drop::drop` runs.
    _lock: std::fs::File,
}

/// Why a reset left the Funnel on.
enum ResetFailure {
    /// :443 may no longer be this start's, so turning it off could remove someone else's.
    Skipped(&'static str),
    /// `off` itself failed.
    Failed(String),
}

/// Whether :443 still holds only what this start installed, read just before turning it off.
#[derive(Debug, PartialEq, Eq)]
enum FunnelOwnership {
    /// Only `/` to this start's port: this start's to turn off.
    Ours,
    /// Nothing on :443, so there's nothing to turn off.
    Gone,
    /// Something else is on :443 now, such as another aoe daemon's start that succeeded since.
    Changed,
    /// The status couldn't be read or parsed.
    Unknown,
}

/// The ownership a `tailscale funnel status --json` run shows for `port`.
fn funnel_ownership(status: Option<&std::process::Output>, port: u16) -> FunnelOwnership {
    let Some(parsed) = status
        .filter(|out| out.status.success())
        .and_then(|out| serde_json::from_slice::<serde_json::Value>(&out.stdout).ok())
    else {
        return FunnelOwnership::Unknown;
    };
    let on_443 = |key: &str| key == "443" || key.ends_with(":443");
    let ours = format!("127.0.0.1:{port}");
    let mut routes = 0;
    for (vhost, cfg) in parsed
        .get("Web")
        .and_then(|w| w.as_object())
        .into_iter()
        .flatten()
    {
        if !on_443(vhost) {
            continue;
        }
        for (path, handler) in cfg
            .get("Handlers")
            .and_then(|h| h.as_object())
            .into_iter()
            .flatten()
        {
            let proxy = handler
                .get("Proxy")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let authority = proxy.split_once("://").map_or(proxy, |(_, rest)| rest);
            if path != "/" || authority.split(['/', '?', '#']).next() != Some(ours.as_str()) {
                return FunnelOwnership::Changed;
            }
            routes += 1;
        }
    }
    let any_443 = ["TCP", "Web", "AllowFunnel"].iter().any(|field| {
        parsed
            .get(*field)
            .and_then(|v| v.as_object())
            .is_some_and(|m| m.keys().any(|k| on_443(k)))
    });
    match (routes, any_443) {
        (1, _) => FunnelOwnership::Ours,
        (0, false) => FunnelOwnership::Gone,
        _ => FunnelOwnership::Changed,
    }
}

/// The skip reason for an ownership that isn't this start's, or None to go ahead.
fn skip_reason(ownership: &FunnelOwnership) -> Option<&'static str> {
    match ownership {
        FunnelOwnership::Ours | FunnelOwnership::Gone => None,
        FunnelOwnership::Changed => Some(
            "Port 443 changed after this start turned Tailscale Funnel on (another aoe daemon may \
             have started since), so it wasn't turned off",
        ),
        FunnelOwnership::Unknown => Some(
            "Couldn't read `tailscale funnel status` to confirm the Funnel was still this start's, \
             so it wasn't turned off",
        ),
    }
}

impl FunnelStartGuard {
    fn new(program: &std::ffi::OsStr, port: u16, armed: bool, lock: std::fs::File) -> Self {
        Self {
            program: program.to_owned(),
            port,
            armed,
            _lock: lock,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }

    /// The start failed after `--bg`. Turn the Funnel back off if it's this start's, and say in
    /// the error what was left on, since the user may still have a public Funnel.
    async fn fail(&mut self, error: anyhow::Error) -> anyhow::Error {
        if !self.armed {
            return anyhow::anyhow!(
                "{error:#}. Port 443 was already a public Tailscale Funnel before this start, so \
                 it wasn't turned off and may still route `/` to this port; check `tailscale \
                 funnel status`"
            );
        }
        match self.reset().await {
            Ok(()) => error,
            Err(ResetFailure::Skipped(why)) => {
                anyhow::anyhow!("{error:#}. {why}; check `tailscale funnel status`")
            }
            Err(ResetFailure::Failed(why)) => anyhow::anyhow!(
                "{error:#}. Couldn't turn the Tailscale Funnel back off ({why}); run \
                 `tailscale funnel --https=443 off`"
            ),
        }
    }

    /// Turn the Funnel back off and wait for it, bounded; the Daemon may exit right after.
    /// It's turned off only if :443 still holds just this start's route, read right before.
    async fn reset(&mut self) -> Result<(), ResetFailure> {
        if !self.armed {
            return Ok(());
        }
        let status = Command::new(&self.program)
            .args(["funnel", "status", "--json"])
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output();
        let status = tokio::time::timeout(FUNNEL_OFF_TIMEOUT, status).await;
        let ownership = funnel_ownership(status.ok().and_then(|r| r.ok()).as_ref(), self.port);
        if ownership != FunnelOwnership::Ours {
            self.armed = false;
            return match skip_reason(&ownership) {
                None => Ok(()),
                Some(why) => {
                    warn!(target: "serve.tunnel", ?ownership, "Left the Tailscale Funnel on: {why}");
                    Err(ResetFailure::Skipped(why))
                }
            };
        }
        let off = Command::new(&self.program)
            .args(FUNNEL_OFF_ARGS)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output();
        let result = tokio::time::timeout(FUNNEL_OFF_TIMEOUT, off).await;
        // Disarm only now: a start cancelled mid-reset kills `off`, so Drop must send it again.
        self.armed = false;
        let why = match result {
            Ok(Ok(out)) if out.status.success() => {
                info!(target: "serve.tunnel", "Turned off the Tailscale Funnel this start switched on");
                return Ok(());
            }
            Ok(Ok(out)) => format!(
                "exit {:?}: {}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr).trim()
            ),
            Ok(Err(e)) => e.to_string(),
            Err(_) => format!("timed out after {}s", FUNNEL_OFF_TIMEOUT.as_secs()),
        };
        warn!(target: "serve.tunnel", %why, "Couldn't turn off the Tailscale Funnel this start switched on");
        Err(ResetFailure::Failed(why))
    }
}

impl Drop for FunnelStartGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // Cancelled mid-start: there's no task to await in, so block here, bounded and reaped
        // by `run_with_timeout`. A detached thread could die with the Daemon before it ran.
        let run = |args: &[&str]| {
            crate::process::run_with_timeout(
                std::process::Command::new(&self.program)
                    .args(args)
                    .stdin(std::process::Stdio::null()),
                FUNNEL_OFF_TIMEOUT,
            )
        };
        let status = run(&["funnel", "status", "--json"]).ok().flatten();
        let ownership = funnel_ownership(status.as_ref(), self.port);
        if ownership != FunnelOwnership::Ours {
            if let Some(why) = skip_reason(&ownership) {
                warn!(
                    target: "serve.tunnel",
                    ?ownership,
                    "Left the Tailscale Funnel on after a cancelled start: {why}"
                );
            }
            return;
        }
        match run(&FUNNEL_OFF_ARGS) {
            Ok(Some(out)) if out.status.success() => {
                info!(target: "serve.tunnel", "Turned off the Tailscale Funnel a cancelled start switched on")
            }
            other => warn!(
                target: "serve.tunnel",
                ?other,
                "Couldn't turn off the Tailscale Funnel a cancelled start switched on"
            ),
        }
    }
}

/// What `tailscale funnel status --json` says about :443 before a start.
#[derive(Debug, PartialEq, Eq)]
enum FunnelPreflight {
    /// Nothing on :443, so everything there after `--bg` is this start's to turn off.
    Unused,
    /// Already a public Funnel, to loopback backends only: `--bg` sets `/` and the rest stays.
    PublicLoopback,
    /// Starting would replace or expose something this start couldn't put back.
    Refuse(String),
}

/// The refusal for a status that couldn't be read: it can't show what `--bg` would change.
fn unreadable_funnel_status() -> FunnelPreflight {
    FunnelPreflight::Refuse(
        "couldn't read `tailscale funnel status --json`, so aoe can't tell whether starting \
         Funnel would replace or expose something on port 443. Check `tailscale funnel status`, \
         or pass --no-tailscale to use Cloudflare."
            .into(),
    )
}

/// The refusal for a :443 handler this start must not replace.
fn conflicting_funnel(existing: String) -> FunnelPreflight {
    FunnelPreflight::Refuse(format!(
        "port 443 is already configured on this node for a different backend ({existing}). Run \
         `tailscale funnel reset` to clear the existing config, or pass --no-tailscale to use \
         Cloudflare."
    ))
}

/// Probe `tailscale funnel status --json` to see if port 443 is already configured.
async fn inspect_existing_funnel(program: &std::ffi::OsStr, local_port: u16) -> FunnelPreflight {
    let out = Command::new(program)
        .args(["funnel", "status", "--json"])
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output()
        .await;
    let Ok(out) = out else {
        return unreadable_funnel_status();
    };
    if !out.status.success() {
        debug!(
            target: "serve.tunnel",
            exit = ?out.status.code(),
            stderr = %String::from_utf8_lossy(&out.stderr).trim(),
            "inspect_existing_funnel: `tailscale funnel status --json` non-zero; state unknown"
        );
        return unreadable_funnel_status();
    }
    let parsed: serde_json::Value = match serde_json::from_slice(&out.stdout) {
        Ok(v) => v,
        Err(e) => {
            debug!(target: "serve.tunnel", error = %e, "inspect_existing_funnel: JSON parse failed; state unknown");
            return unreadable_funnel_status();
        }
    };

    funnel_preflight(&parsed, local_port)
}

/// Read a `tailscale funnel status --json` document for a start on `local_port`.
fn funnel_preflight(parsed: &serde_json::Value, local_port: u16) -> FunnelPreflight {
    let on_443 = |key: &str| key == "443" || key.ends_with(":443");
    let keys = |field: &str| {
        parsed
            .get(field)
            .and_then(|v| v.as_object())
            .into_iter()
            .flat_map(|m| m.keys())
    };
    // Walk the Web map looking for a port-443 handler whose target isn't something we can
    // safely replace.
    let web = parsed.get("Web").and_then(|w| w.as_object());
    for (vhost, cfg) in web.into_iter().flatten() {
        if !vhost.ends_with(":443") {
            continue;
        }
        let Some(handlers) = cfg.get("Handlers").and_then(|h| h.as_object()) else {
            continue;
        };
        for (path, handler) in handlers.iter() {
            if let Some(proxy) = handler.get("Proxy").and_then(|v| v.as_str()) {
                // A loopback backend is this port from an earlier start, a prior aoe run on
                // another port, or another local dev server; overwriting it is the expected
                // behavior, not a conflict.
                if proxy_is_loopback(proxy) {
                    debug!(
                        vhost = %vhost,
                        path = %path,
                        proxy = %proxy,
                        "inspect_existing_funnel: stale loopback proxy, will be replaced"
                    );
                    continue;
                }
                debug!(
                    vhost = %vhost,
                    path = %path,
                    proxy = %proxy,
                    "inspect_existing_funnel: port 443 already held by different proxy backend"
                );
                return conflicting_funnel(format!("proxy {} -> {}", path, proxy));
            } else if let Some(p) = handler.get("Path").and_then(|v| v.as_str()) {
                debug!(
                    vhost = %vhost,
                    path = %path,
                    file_path = %p,
                    "inspect_existing_funnel: port 443 held by file-server handler"
                );
                return conflicting_funnel(format!("file server {} -> {}", path, p));
            } else if handler.get("Text").is_some() {
                debug!(
                    vhost = %vhost,
                    path = %path,
                    "inspect_existing_funnel: port 443 held by static-text handler"
                );
                return conflicting_funnel(format!("static text at {}", path));
            } else {
                debug!(
                    vhost = %vhost,
                    path = %path,
                    handler = ?handler,
                    "inspect_existing_funnel: port 443 held by unknown handler type"
                );
                return conflicting_funnel(format!("unknown handler at {}", path));
            }
        }
    }
    debug!(
        local_port = local_port,
        "inspect_existing_funnel: no conflicting :443 handler"
    );
    if !keys("TCP")
        .chain(keys("Web"))
        .chain(keys("AllowFunnel"))
        .any(|k| on_443(k))
    {
        return FunnelPreflight::Unused;
    }
    let public = parsed
        .get("AllowFunnel")
        .and_then(|v| v.as_object())
        .is_some_and(|m| {
            m.iter()
                .any(|(k, v)| on_443(k) && v.as_bool() == Some(true))
        });
    if public {
        return FunnelPreflight::PublicLoopback;
    }
    // `--bg` turns on Funnel for the whole :443 vhost, so every path there would go public.
    FunnelPreflight::Refuse(
        "port 443 on this node has tailnet-only Tailscale Serve config, and starting Funnel would \
         make all of it public. Check `tailscale serve status` and move that config off :443 (or \
         make it public yourself), or pass --no-tailscale to use Cloudflare."
            .into(),
    )
}

/// Verify this node has the Funnel node-capability set in its ACL.
async fn check_funnel_capability(program: &std::ffi::OsStr) -> anyhow::Result<()> {
    // Tailscale emits the cap key with the allowed-port list appended as a query string,
    // e.g. `?ports=443,8443,10000`.
    const FUNNEL_CAP_PREFIX: &str = "https://tailscale.com/cap/funnel-ports";
    let out = Command::new(program)
        .args(["status", "--json"])
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("could not run `tailscale status --json`: {e}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "`tailscale status --json` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let parsed: serde_json::Value = serde_json::from_slice(&out.stdout)
        .map_err(|e| anyhow::anyhow!("could not parse tailscale status JSON: {e}"))?;
    let cap_map = parsed
        .get("Self")
        .and_then(|s| s.get("CapMap"))
        .and_then(|m| m.as_object());
    let has_cap = cap_map.is_some_and(|m| m.keys().any(|k| k.starts_with(FUNNEL_CAP_PREFIX)));
    debug!(
        has_funnel_cap = has_cap,
        cap_count = cap_map.map(|m| m.len()).unwrap_or(0),
        "check_funnel_capability: inspected Self.CapMap"
    );
    if !has_cap {
        anyhow::bail!("this node is missing the `funnel` nodeAttr in the tailnet ACL");
    }
    Ok(())
}

async fn tailscale_funnel_url(program: &std::ffi::OsStr) -> anyhow::Result<String> {
    let out = Command::new(program)
        .args(["status", "--json"])
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("failed to run tailscale status: {}", e))?;
    debug!(
        exit = ?out.status.code(),
        success = out.status.success(),
        stdout_len = out.stdout.len(),
        stderr = %String::from_utf8_lossy(&out.stderr).trim(),
        "tailscale_funnel_url: `tailscale status --json` returned"
    );
    if !out.status.success() {
        anyhow::bail!(
            "tailscale status --json failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let parsed: serde_json::Value = serde_json::from_slice(&out.stdout).map_err(|e| {
        debug!(
            stdout_head = %String::from_utf8_lossy(&out.stdout).chars().take(200).collect::<String>(),
            "tailscale_funnel_url: status JSON parse failed"
        );
        anyhow::anyhow!("parse tailscale status JSON: {}", e)
    })?;
    let dns = parsed
        .get("Self")
        .and_then(|s| s.get("DNSName"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            debug!(
                keys = ?parsed.as_object().map(|o| o.keys().collect::<Vec<_>>()),
                "tailscale_funnel_url: Self.DNSName missing; dumping top-level keys"
            );
            anyhow::anyhow!("Self.DNSName missing from tailscale status")
        })?;
    let host = dns.trim_end_matches('.');
    if host.is_empty() {
        anyhow::bail!("empty DNSName from tailscale status");
    }
    debug!(host = %host, "tailscale_funnel_url: resolved stable hostname");
    Ok(format!("https://{}", host))
}

/// Does this ServeConfig proxy URL point at the local machine (127.0.0.1, localhost, or
/// ::1)?
fn proxy_is_loopback(proxy: &str) -> bool {
    let lower = proxy.to_ascii_lowercase();
    let after_scheme = lower
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(&lower);
    // IPv6 is bracketed.
    if let Some(rest) = after_scheme.strip_prefix('[') {
        return rest.starts_with("::1]");
    }
    let host = after_scheme
        .split(['/', ':'])
        .next()
        .unwrap_or(after_scheme);
    matches!(host, "127.0.0.1" | "localhost")
}

/// Parse a `https://login.tailscale.com/f/funnel?...` activation URL out of a `tailscale
/// funnel` stderr line.
fn extract_funnel_activation_url(line: &str) -> Option<String> {
    const PREFIX: &str = "https://login.tailscale.com/f/funnel";
    let idx = line.find(PREFIX)?;
    let rest = &line[idx..];
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    Some(rest[..end].to_string())
}

/// Extract a trycloudflare.com tunnel URL from a cloudflared stderr line.
fn extract_tunnel_url(line: &str) -> Option<String> {
    for word in line.split_whitespace() {
        if word.starts_with("https://") && word.contains(".trycloudflare.com") {
            // Trim trailing punctuation that may appear in log output.
            if let Some(pos) = word.find(".trycloudflare.com") {
                let end = pos + ".trycloudflare.com".len();
                return Some(word[..end].to_string());
            }
        }
    }
    None
}

/// Render a QR code to stderr for easy phone scanning.
#[cfg(not(feature = "web"))]
pub fn print_qr_code(url: &str) {
    eprintln!();
    eprintln!("  Open: {}", url);
    eprintln!();
}

/// Render a QR code to stderr for easy phone scanning.
#[cfg(feature = "web")]
pub fn print_qr_code(url: &str) {
    use qrcode::QrCode;

    match QrCode::new(url.as_bytes()) {
        Ok(code) => {
            let string = code
                .render::<char>()
                .quiet_zone(true)
                .module_dimensions(2, 1)
                .build();
            eprintln!();
            for line in string.lines() {
                eprintln!("  {}", line);
            }
            eprintln!("  ^^ Scan this QR code to connect from your phone.");
            eprintln!("     (Resize your terminal wider if it looks garbled.)");
            eprintln!();
            eprintln!("  Or open: {}", url);
            eprintln!();
        }
        Err(e) => {
            eprintln!("Could not generate QR code: {}", e);
            eprintln!("Open this URL: {}", url);
        }
    }
}

/// Check if cloudflared is installed and accessible on PATH.
pub fn check_cloudflared() -> anyhow::Result<()> {
    match std::process::Command::new("cloudflared")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
    {
        Ok(status) if status.success() => Ok(()),
        _ => anyhow::bail!(
            "cloudflared is not installed or not on PATH.\n\
             Install it: https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/downloads/\n\
             \n\
             Quick install:\n\
             - macOS:  brew install cloudflared\n\
             - Linux:  sudo apt install cloudflared\n\
             - Other:  see the URL above"
        ),
    }
}

/// How long a `tailscale` availability probe may take. A healthy CLI answers in well under a
/// second; a wedged `tailscaled` must not hang `aoe serve`'s pre-flight, freeze the TUI when R
/// opens the view, or hold the Daemon's tunnel selection until its startup deadline.
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// Sync counterpart of `tailscale_probe()` for use from the TUI (which avoids spinning
/// up a tokio runtime just to probe for a CLI).
pub fn tailscale_available_sync() -> bool {
    tailscale_available_sync_with(std::ffi::OsStr::new("tailscale"), PROBE_TIMEOUT)
}

fn tailscale_available_sync_with(program: &std::ffi::OsStr, timeout: std::time::Duration) -> bool {
    // Unlike the async path, this runs on the TUI render hot path, so we keep it cheap and
    // quiet on the happy path.
    probe_succeeds(program, "--version", timeout) && probe_succeeds(program, "status", timeout)
}

/// Whether `program arg` exits zero within `timeout`; a probe still running is killed.
fn probe_succeeds(program: &std::ffi::OsStr, arg: &str, timeout: std::time::Duration) -> bool {
    let spawned = std::process::Command::new(program)
        .arg(arg)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    let status =
        spawned.and_then(|mut child| crate::process::wait_with_timeout(&mut child, timeout));
    match status {
        Ok(Some(status)) if status.success() => true,
        Ok(Some(status)) => {
            debug!(exit = ?status.code(), "tailscale_available_sync: `tailscale {arg}` failed");
            false
        }
        Ok(None) => {
            debug!(
                timeout_s = timeout.as_secs_f32(),
                "tailscale_available_sync: `tailscale {arg}` timed out; treating Tailscale as unavailable"
            );
            false
        }
        Err(error) => {
            debug!(%error, "tailscale_available_sync: `tailscale {arg}` could not run");
            false
        }
    }
}

/// Sync counterpart of `check_funnel_capability()`.
pub fn tailscale_funnel_cap_ready_sync() -> bool {
    // Tailscale emits the cap key with the allowed-port list appended as a query string,
    // e.g. `?ports=443,8443,10000`.
    const FUNNEL_CAP_PREFIX: &str = "https://tailscale.com/cap/funnel-ports";
    let out = crate::process::run_with_timeout(
        std::process::Command::new("tailscale")
            .args(["status", "--json"])
            .stdin(std::process::Stdio::null()),
        PROBE_TIMEOUT,
    );
    let Ok(Some(out)) = out else {
        debug!("tailscale_funnel_cap_ready_sync: spawn failed or timed out");
        return false;
    };
    if !out.status.success() {
        debug!(
            exit = ?out.status.code(),
            "tailscale_funnel_cap_ready_sync: tailscale status --json non-zero"
        );
        return false;
    }
    let Ok(parsed): Result<serde_json::Value, _> = serde_json::from_slice(&out.stdout) else {
        debug!("tailscale_funnel_cap_ready_sync: JSON parse failed");
        return false;
    };
    let self_obj = parsed.get("Self");
    let cap_map = self_obj
        .and_then(|s| s.get("CapMap"))
        .and_then(|m| m.as_object());
    let has_cap = cap_map.is_some_and(|m| m.keys().any(|k| k.starts_with(FUNNEL_CAP_PREFIX)));
    debug!(
        has_funnel_cap = has_cap,
        cap_count = cap_map.map(|m| m.len()).unwrap_or(0),
        "tailscale_funnel_cap_ready_sync: inspected Self.CapMap"
    );
    has_cap
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_cloudflare_tunnels_are_trusted_for_cf_connecting_ip() {
        for (kind, trusted) in [
            (TunnelKind::Quick, true),
            (
                TunnelKind::Named {
                    tunnel_name: "t".into(),
                },
                true,
            ),
            (TunnelKind::Tailscale, false),
        ] {
            assert_eq!(
                kind.sets_cf_connecting_ip(),
                trusted,
                "{}",
                kind.mode_label()
            );
        }
    }

    use super::*;

    #[test]
    fn funnel_preflight_starts_only_where_it_can_put_back_what_it_changes() {
        let web =
            |to: &str| serde_json::json!({"node.ts.net:443": {"Handlers": {"/": {"Proxy": to}}}});
        let public = serde_json::json!({"node.ts.net:443": true});
        // (status document, the outcome: "unused", "public", or a phrase of the refusal)
        let cases = [
            (serde_json::json!({}), "unused"),
            // Other ports don't count.
            (
                serde_json::json!({"TCP": {"8443": {"HTTPS": true}}, "Web": {"node.ts.net:8443": {"Handlers": {"/": {"Proxy": "http://127.0.0.1:3000"}}}}}),
                "unused",
            ),
            // A public Funnel to this port or another loopback port: `--bg` replaces `/`.
            (
                serde_json::json!({"Web": web("http://127.0.0.1:8081"), "AllowFunnel": public}),
                "public",
            ),
            (
                serde_json::json!({"Web": web("http://127.0.0.1:9999"), "AllowFunnel": public}),
                "public",
            ),
            (serde_json::json!({"AllowFunnel": public}), "public"),
            // Tailnet-only Serve: `--bg` would make every path on :443 public.
            (
                serde_json::json!({"Web": web("http://127.0.0.1:8081")}),
                "tailnet-only",
            ),
            (
                serde_json::json!({"TCP": {"443": {"HTTPS": true}}}),
                "tailnet-only",
            ),
            (
                serde_json::json!({"Web": web("http://127.0.0.1:9000"), "AllowFunnel": {"node.ts.net:443": false}}),
                "tailnet-only",
            ),
            (
                serde_json::json!({"Web": web("http://10.0.0.5:80"), "AllowFunnel": public}),
                "different backend (proxy / -> http://10.0.0.5:80)",
            ),
        ];
        for (doc, want) in cases {
            let got = funnel_preflight(&doc, 8081);
            match (&got, want) {
                (FunnelPreflight::Unused, "unused")
                | (FunnelPreflight::PublicLoopback, "public") => {}
                (FunnelPreflight::Refuse(why), phrase) if why.contains(phrase) => {}
                _ => panic!("{doc}: {got:?}, want {want}"),
            }
        }
    }

    #[test]
    fn a_reset_turns_off_only_a_funnel_that_is_still_this_starts() {
        let out = |code: i32, stdout: &str| {
            use std::os::unix::process::ExitStatusExt;
            std::process::Output {
                status: std::process::ExitStatus::from_raw(code << 8),
                stdout: stdout.as_bytes().to_vec(),
                stderr: Vec::new(),
            }
        };
        let route = |to: &str| {
            format!(
                r#"{{"TCP":{{"443":{{"HTTPS":true}}}},"Web":{{"node.ts.net:443":{{"Handlers":{{"/":{{"Proxy":"{to}"}}}}}}}},"AllowFunnel":{{"node.ts.net:443":true}}}}"#
            )
        };
        let ours = route("http://127.0.0.1:8081");
        let newer = route("http://127.0.0.1:8080");
        let sibling = r#"{"Web":{"node.ts.net:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:8081"},"/blog":{"Proxy":"http://127.0.0.1:3000"}}}}}"#;
        let cases = [
            (Some(out(0, &ours)), FunnelOwnership::Ours),
            (Some(out(0, "{}")), FunnelOwnership::Gone),
            // A newer start replaced `/`, or added a route beside it.
            (Some(out(0, &newer)), FunnelOwnership::Changed),
            (Some(out(0, sibling)), FunnelOwnership::Changed),
            (
                Some(out(0, r#"{"AllowFunnel":{"node.ts.net:443":true}}"#)),
                FunnelOwnership::Changed,
            ),
            (Some(out(1, &ours)), FunnelOwnership::Unknown),
            (Some(out(0, "not json")), FunnelOwnership::Unknown),
            (None, FunnelOwnership::Unknown),
        ];
        for (status, want) in cases {
            assert_eq!(funnel_ownership(status.as_ref(), 8081), want, "{status:?}");
        }
    }

    /// A fake `tailscale` that logs each call's arguments and keeps a Funnel state file.
    /// Each start's first `status --json` is its capability check and its second the URL read,
    /// shaped by `url_read` (which can read the call count `$n`). `funnel status --json` prints the state (`None` makes it fail),
    /// `funnel --bg` runs `bg_before`, makes :443 public as the real one does, touches
    /// `bg-applied`, and then runs `bg_exit`, and
    /// `funnel --https=443 off` runs `off` and then empties the state.
    #[cfg(unix)]
    fn fake_tailscale(
        dir: &std::path::Path,
        funnel_state: Option<&str>,
        url_read: &str,
        off: &str,
        bg_exit: &str,
        bg_before: &str,
    ) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let fake = dir.join("tailscale");
        let log = dir.join("argv.log");
        let count = dir.join("status-count");
        let state = dir.join("funnel-state");
        std::fs::write(&state, funnel_state.unwrap_or("{}")).unwrap();
        let funnel_status = match funnel_state {
            Some(_) => format!("cat '{}'", state.display()),
            None => "exit 1".to_owned(),
        };
        let script = format!(
            r#"#!/bin/sh
echo "$*" >> '{log}'
case "$*" in
  "status --json")
    n=$(cat '{count}' 2>/dev/null || echo 0); n=$((n+1)); echo $n > '{count}'
    if [ $((n % 2)) = 1 ]; then
      echo '{{"Self":{{"CapMap":{{"https://tailscale.com/cap/funnel-ports?ports=443":[]}},"DNSName":"node.ts.net."}}}}'
      exit 0
    fi
    {url_read} ;;
  "funnel status --json") {funnel_status} ;;
  "funnel --bg --yes "*)
    {bg_before}
    if [ "$(cat '{state}')" = '{{}}' ]; then
      echo "{{\"Web\":{{\"node.ts.net:443\":{{\"Handlers\":{{\"/\":{{\"Proxy\":\"http://127.0.0.1:$4\"}}}}}}}},\"AllowFunnel\":{{\"node.ts.net:443\":true}}}}" > '{state}'
    elif ! grep -q '"node.ts.net:443":true' '{state}'; then
      exposed=$(sed 's/^{{/{{"AllowFunnel":{{"node.ts.net:443":true}},/' '{state}')
      echo "$exposed" > '{state}'
    fi
    touch '{applied}'
    {bg_exit} ;;
  "funnel --https=443 off")
    {off}
    echo '{{}}' > '{state}' ;;
  *) exit 0 ;;
esac
"#,
            log = log.display(),
            count = count.display(),
            state = state.display(),
            applied = dir.join("bg-applied").display(),
        );
        std::fs::write(&fake, script).unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        fake
    }

    #[cfg(unix)]
    fn argv_log(dir: &std::path::Path) -> Vec<String> {
        std::fs::read_to_string(dir.join("argv.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_failed_tailscale_start_leaves_port_443_as_exposed_as_it_found_it() {
        const OFF: &str = "funnel --https=443 off";
        const BG: &str = "funnel --bg --yes 8081";
        let ours = r#"{"Web":{"node.ts.net:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:8081"}}}},"AllowFunnel":{"node.ts.net:443":true}}"#;
        let sibling = r#"{"Web":{"node.ts.net:443":{"Handlers":{"/blog":{"Proxy":"http://127.0.0.1:3000"}}}},"AllowFunnel":{"node.ts.net:443":true}}"#;
        let private = r#"{"TCP":{"443":{"HTTPS":true}},"Web":{"node.ts.net:443":{"Handlers":{"/admin":{"Proxy":"http://127.0.0.1:9000"}}}}}"#;
        let ok_url = r#"echo '{"Self":{"DNSName":"node.ts.net."}}'"#;
        struct Row<'a> {
            state: Option<&'a str>,
            url_read: &'a str,
            off_body: &'a str,
            bg_exit: &'a str,
            /// None for success, else a phrase the error must hold.
            err: Option<&'a str>,
            bg: bool,
            offs: usize,
            /// :443 public after the start, and a route that must survive it.
            public_after: bool,
            keeps: &'a str,
        }
        let row = |state, url_read, err, bg, offs, public_after, keeps| Row {
            state,
            url_read,
            off_body: "",
            bg_exit: "",
            err,
            bg,
            offs,
            public_after,
            keeps,
        };
        let cases = [
            // This start made :443 public, so a failure turns it back off.
            row(
                Some("{}"),
                "exit 1",
                Some("tailscale status"),
                true,
                1,
                false,
                "",
            ),
            // The reset itself fails, so the error says how to finish it.
            Row {
                off_body: "exit 1",
                ..row(
                    Some("{}"),
                    "exit 1",
                    Some("run `tailscale funnel --https=443 off`"),
                    true,
                    1,
                    true,
                    "",
                )
            },
            // `--bg` applied the config but then failed: still this start's to turn off.
            Row {
                bg_exit: "exit 1",
                ..row(
                    Some("{}"),
                    "exit 1",
                    Some("tailscale funnel exited with status"),
                    true,
                    1,
                    false,
                    "",
                )
            },
            // ... and when that `off` fails too, the error says how to finish it.
            Row {
                bg_exit: "exit 1",
                off_body: "exit 1",
                ..row(
                    Some("{}"),
                    "exit 1",
                    Some("run `tailscale funnel --https=443 off`"),
                    true,
                    1,
                    true,
                    "",
                )
            },
            // Already public: only `/` changed, and the other routes stay.
            row(
                Some(ours),
                "exit 1",
                Some("already a public Tailscale Funnel"),
                true,
                0,
                true,
                "8081",
            ),
            row(
                Some(sibling),
                "exit 1",
                Some("already a public Tailscale Funnel"),
                true,
                0,
                true,
                "/blog",
            ),
            // Tailnet-only Serve, or a status that couldn't be read: refused before `--bg`.
            row(
                Some(private),
                "exit 1",
                Some("tailnet-only"),
                false,
                0,
                false,
                "/admin",
            ),
            row(
                None,
                "exit 1",
                Some("couldn't read `tailscale funnel status --json`"),
                false,
                0,
                false,
                "",
            ),
            row(
                Some("not json"),
                "exit 1",
                Some("couldn't read"),
                false,
                0,
                false,
                "not json",
            ),
            row(Some("{}"), ok_url, None, true, 0, true, ""),
        ];
        for r in cases {
            let dir = tempfile::tempdir().unwrap();
            let fake = fake_tailscale(dir.path(), r.state, r.url_read, r.off_body, r.bg_exit, "");
            let lock = dir.path().join("funnel.lock");
            let result = TunnelHandle::spawn_tailscale_with(fake.as_os_str(), &lock, 8081).await;
            let name = format!("{:?} / {} / {}", r.state, r.url_read, r.off_body);
            match (&result, r.err) {
                (Ok(_), None) => {}
                (Err(e), Some(phrase)) => assert!(e.to_string().contains(phrase), "{name}: {e}"),
                (other, _) => panic!("{name}: {:?}", other.as_ref().map(|_| "started")),
            }
            // Awaited before the error returns, not left to a thread that may die with the Daemon.
            let calls = argv_log(dir.path());
            assert_eq!(calls.iter().any(|c| c == BG), r.bg, "{name}: {calls:?}");
            assert_eq!(
                calls.iter().filter(|c| *c == OFF).count(),
                r.offs,
                "{name}: {calls:?}"
            );
            let after = std::fs::read_to_string(dir.path().join("funnel-state")).unwrap();
            assert_eq!(
                after.contains(r#""node.ts.net:443":true"#),
                r.public_after,
                "{name}: {after}"
            );
            assert!(after.contains(r.keeps), "{name}: {after}");
        }
    }

    #[cfg(unix)]
    // The test polls the fake's log with real sleeps, so the start needs its own worker.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cancelled_tailscale_start_turns_off_the_funnel_it_switched_on() {
        const OFF: &str = "funnel --https=443 off";
        let count = |calls: &[String], want: &str| calls.iter().filter(|c| *c == want).count();
        // (where the start is blocked when it's cancelled, `off` calls expected)
        let cases = [
            // Cancelled while `--bg` runs: after it applied the config, or before (nothing to undo).
            ("bg", 1),
            ("bg before apply", 0),
            ("url read", 1),
            // The cancel kills the awaited `off`, which may not have run, so Drop sends another.
            ("reset", 2),
        ];
        for (blocked_in, offs) in cases {
            let dir = tempfile::tempdir().unwrap();
            let release = dir.path().join("release");
            let block = format!(
                "while [ ! -f '{}' ]; do sleep 0.05; done",
                release.display()
            );
            let once = dir.path().join("off-once");
            let once = once.display();
            let started = dir.path().join("bg-started");
            let bg_before = if blocked_in == "bg before apply" {
                format!("touch '{}'; {block}", started.display())
            } else {
                String::new()
            };
            let (url_read, off, bg_exit) = match blocked_in {
                "bg" => ("exit 1".to_owned(), String::new(), block.clone()),
                "bg before apply" => ("exit 1".to_owned(), String::new(), String::new()),
                "url read" => (format!("{block}; exit 1"), String::new(), String::new()),
                _ => (
                    "exit 1".to_owned(),
                    format!("if [ ! -f '{once}' ]; then touch '{once}'; {block}; fi"),
                    String::new(),
                ),
            };
            let fake = fake_tailscale(
                dir.path(),
                Some("{}"),
                &url_read,
                &off,
                &bg_exit,
                &bg_before,
            );
            let program = fake.clone();
            let lock = dir.path().join("funnel.lock");
            let start = tokio::spawn(async move {
                TunnelHandle::spawn_tailscale_with(program.as_os_str(), &lock, 8081).await
            });

            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            let before = loop {
                let calls = argv_log(dir.path());
                let ready = match blocked_in {
                    // Markers: the config is applied, or the first `off` is under way.
                    "bg" => dir.path().join("bg-applied").exists(),
                    "bg before apply" => started.exists(),
                    "url read" => count(&calls, "status --json") >= 2,
                    _ => dir.path().join("off-once").exists(),
                };
                if ready {
                    break calls;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "{blocked_in}: stuck at {calls:?}"
                );
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            };
            assert!(
                before.iter().any(|c| c == "funnel --bg --yes 8081"),
                "{before:?}"
            );
            start.abort();
            // The join resolves once the future is dropped, so Drop's `off` has already run:
            // nothing is left to a thread that could die with the Daemon.
            assert!(start.await.is_err_and(|e| e.is_cancelled()));
            let after = argv_log(dir.path());
            let state = std::fs::read_to_string(dir.path().join("funnel-state")).unwrap();
            std::fs::write(&release, "").unwrap();
            assert_eq!(count(&after, OFF), offs, "{blocked_in}: {after:?}");
            // Turned off, or never applied: either way :443 is empty again.
            assert_eq!(state.trim(), "{}", "{blocked_in}");
        }
    }

    #[test]
    fn only_a_timed_out_probe_refuses_to_pick_a_transport() {
        // (probe, start Funnel, or the error names the way out)
        for (probe, want) in [
            (TailscaleProbe::Available, Some(true)),
            (TailscaleProbe::Unavailable, Some(false)),
            (TailscaleProbe::TimedOut, None),
        ] {
            match probe.use_funnel() {
                Ok(funnel) => assert_eq!(Some(funnel), want, "{probe:?}"),
                Err(e) => {
                    assert_eq!(want, None, "{probe:?}");
                    assert!(e.to_string().contains("--no-tailscale"), "{e}");
                }
            }
        }
    }

    #[cfg(unix)]
    // The test polls the fake's log with real sleeps, so the start needs its own worker.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_older_failed_start_leaves_a_newer_starts_funnel_alone() {
        const OFF: &str = "funnel --https=443 off";
        let newer = r#"{"Web":{"node.ts.net:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:8080"}}}},"AllowFunnel":{"node.ts.net:443":true}}"#;
        // How the older start ends after the newer one took :443.
        for cancelled in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let release = dir.path().join("release");
            let url_read = format!(
                "while [ ! -f '{}' ]; do sleep 0.05; done; exit 1",
                release.display()
            );
            let fake = fake_tailscale(dir.path(), Some("{}"), &url_read, "", "", "");
            let program = fake.clone();
            let lock = dir.path().join("funnel.lock");
            // Start A switches Funnel on for 8081, then waits on its URL read.
            let start = tokio::spawn(async move {
                TunnelHandle::spawn_tailscale_with(program.as_os_str(), &lock, 8081).await
            });
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while argv_log(dir.path())
                .iter()
                .filter(|c| *c == "status --json")
                .count()
                < 2
            {
                assert!(
                    std::time::Instant::now() < deadline,
                    "A never reached its URL read"
                );
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            // Start B, another daemon on 8080, takes `/` and succeeds.
            std::fs::write(dir.path().join("funnel-state"), newer).unwrap();
            let result = if cancelled {
                start.abort();
                assert!(start.await.is_err_and(|e| e.is_cancelled()));
                None
            } else {
                std::fs::write(&release, "").unwrap();
                Some(start.await.unwrap())
            };
            std::fs::write(&release, "").unwrap();
            let calls = argv_log(dir.path());
            assert_eq!(
                calls.iter().filter(|c| *c == OFF).count(),
                0,
                "{cancelled}: {calls:?}"
            );
            let after = std::fs::read_to_string(dir.path().join("funnel-state")).unwrap();
            assert_eq!(after, newer, "{cancelled}");
            if let Some(result) = result {
                let e = result.err().expect("A fails");
                assert!(e.to_string().contains("Port 443 changed"), "{e}");
            }
        }
    }

    #[cfg(unix)]
    // The test polls the fake's log with real sleeps, so the starts need their own workers.
    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn a_start_waits_for_another_daemons_reset_before_turning_funnel_on() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("argv.log");
        let release = dir.path().join("release");
        // Start A's URL read fails; start B's (the 4th `status --json`) succeeds.
        let url_read =
            r#"if [ "$n" = 2 ]; then exit 1; fi; echo '{"Self":{"DNSName":"node.ts.net."}}'"#;
        // A's `off` pauses after its ownership read and before it changes anything.
        let off = format!(
            "touch '{entered}'; while [ ! -f '{release}' ]; do sleep 0.05; done; echo 'off done' >> '{log}'",
            entered = dir.path().join("off-entered").display(),
            release = release.display(),
            log = log.display(),
        );
        let fake = fake_tailscale(dir.path(), Some("{}"), url_read, &off, "", "");
        let lock = dir.path().join("funnel.lock");
        let start = |port: u16| {
            let (program, lock) = (fake.clone(), lock.clone());
            tokio::spawn(async move {
                TunnelHandle::spawn_tailscale_with(program.as_os_str(), &lock, port).await
            })
        };
        let wait = |what: &str, done: &dyn Fn() -> bool| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !done() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "no {what}: {:?}",
                    argv_log(dir.path())
                );
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        };
        let a = start(8081);
        wait("A's off", &|| dir.path().join("off-entered").exists());
        // B is really waiting on the lock A holds, not just on its way to it.
        let b = start(8080);
        wait("B blocked on the lock", &|| {
            CONTENDED_FUNNEL_LOCKS
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(&lock)
        });
        std::fs::write(&release, "").unwrap();
        assert!(a.await.unwrap().is_err());
        let b = b.await.unwrap().expect("B starts once A's reset is done");
        assert_eq!(b.url, "https://node.ts.net");

        let calls = argv_log(dir.path());
        let at = |want: &str| calls.iter().position(|c| c == want).unwrap_or(usize::MAX);
        // B's pre-flight (the third `funnel status`, after A's pre-flight and reset read) and its
        // `--bg` both come after A's `off` finished.
        let b_preflight = calls
            .iter()
            .enumerate()
            .filter(|(_, c)| *c == "funnel status --json")
            .nth(2)
            .map_or(usize::MAX, |(i, _)| i);
        assert!(at("off done") < b_preflight, "{calls:?}");
        assert!(at("off done") < at("funnel --bg --yes 8080"), "{calls:?}");
        let state = std::fs::read_to_string(dir.path().join("funnel-state")).unwrap();
        assert!(
            state.contains("127.0.0.1:8080") && !state.contains("8081"),
            "{state}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_wedged_tailscale_times_out_within_the_probe_bound() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        // (script body, sync availability, async probe)
        let cases = [
            ("exit 0", true, TailscaleProbe::Available),
            // Not logged in.
            (
                "[ \"$1\" = status ] && exit 1; exit 0",
                false,
                TailscaleProbe::Unavailable,
            ),
            // Wedged tailscaled: the TUI just hides the option, but the Daemon must not take it
            // for "unavailable" and fall back to Cloudflare.
            (
                "[ \"$1\" = status ] && exec sleep 600; exit 0",
                false,
                TailscaleProbe::TimedOut,
            ),
            ("exec sleep 600", false, TailscaleProbe::TimedOut),
        ];
        for (i, (body, expected, probe)) in cases.into_iter().enumerate() {
            let fake = tmp.path().join(format!("tailscale-{i}"));
            std::fs::write(&fake, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
            let timeout = std::time::Duration::from_millis(300);
            let started = std::time::Instant::now();
            assert_eq!(
                tailscale_available_sync_with(fake.as_os_str(), timeout),
                expected,
                "sync: {body}"
            );
            assert!(
                started.elapsed() < std::time::Duration::from_secs(3),
                "sync: {body}: took {:?}",
                started.elapsed()
            );
            // The Daemon's async probe gets the same bound.
            let started = std::time::Instant::now();
            assert_eq!(
                runtime.block_on(tailscale_probe_with(fake.as_os_str(), timeout)),
                probe,
                "async: {body}"
            );
            assert!(
                started.elapsed() < std::time::Duration::from_secs(3),
                "async: {body}: took {:?}",
                started.elapsed()
            );
        }
    }

    // The telemetry `serve_mode` signal reads `mode_label()`, so a named
    // Cloudflare tunnel must bucket to "tunnel" rather than leak the configured
    // tunnel name (#1885).
    #[test]
    fn mode_label_is_closed_and_never_leaks_tunnel_name() {
        assert_eq!(TunnelKind::Quick.mode_label(), "tunnel");
        assert_eq!(
            TunnelKind::Named {
                tunnel_name: "my-secret-corp-tunnel".to_string(),
            }
            .mode_label(),
            "tunnel",
        );
        assert_eq!(TunnelKind::Tailscale.mode_label(), "tailscale");
    }

    #[test]
    fn extracts_tunnel_and_funnel_urls_from_tool_output() {
        let tunnel = [
            (
                "2026-04-12T12:00:00Z INF +-------------------------------------------------------------------+",
                None,
            ),
            (
                "2026-04-12T12:00:01Z INF |  https://random-words-here.trycloudflare.com  |",
                Some("https://random-words-here.trycloudflare.com"),
            ),
            ("INF Starting tunnel subsystem", None),
            ("https://example.com not a tunnel", None),
            (
                "Visit https://abc-def.trycloudflare.com.",
                Some("https://abc-def.trycloudflare.com"),
            ),
        ];
        for (line, want) in tunnel {
            assert_eq!(extract_tunnel_url(line).as_deref(), want, "{line}");
        }
        let funnel = [
            (
                "         https://login.tailscale.com/f/funnel?node=n6ADBuFYMT11CNTRL",
                Some("https://login.tailscale.com/f/funnel?node=n6ADBuFYMT11CNTRL"),
            ),
            (
                "         https://login.tailscale.com/f/funnel?node=abc   ",
                Some("https://login.tailscale.com/f/funnel?node=abc"),
            ),
            ("https://login.tailscale.com/admin", None),
            ("no url here", None),
        ];
        for (line, want) in funnel {
            assert_eq!(
                extract_funnel_activation_url(line).as_deref(),
                want,
                "{line}"
            );
        }
    }

    #[test]
    fn proxy_is_loopback_accepts_only_local_forms() {
        for (url, want) in [
            ("http://127.0.0.1:8080", true),
            ("http://localhost:3000", true),
            ("https://127.0.0.1", true),
            ("http://[::1]:8080", true),
            ("http://100.64.0.1:8080", false),
            ("http://example.com", false),
            ("http://192.168.1.5:8080", false),
        ] {
            assert_eq!(proxy_is_loopback(url), want, "{url}");
        }
    }
}
