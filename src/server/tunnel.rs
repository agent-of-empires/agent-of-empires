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
        Self::spawn_tailscale_with(std::ffi::OsStr::new("tailscale"), local_port).await
    }

    async fn spawn_tailscale_with(
        program: &std::ffi::OsStr,
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

        // Pre-flight 2.
        let preflight = inspect_existing_funnel(program, local_port).await;
        if let Some(existing) = preflight.conflicting {
            anyhow::bail!(
                "port 443 is already configured on this node for a different \
                 backend ({}). Run `tailscale funnel reset` to clear the \
                 existing config, or pass --no-tailscale to use Cloudflare.",
                existing
            );
        }

        // Single-command Funnel (Tailscale 1.52+).
        let funnel_arg = local_port.to_string();
        let funnel_args = ["funnel", "--bg", "--yes", &funnel_arg];
        info!(
            "Running `tailscale funnel --bg --yes {}` (first-time HTTPS cert provisioning can take 30-60s)",
            local_port
        );

        let mut child = Command::new(program)
            .args(funnel_args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| anyhow::anyhow!("failed to run tailscale funnel: {}", e))?;
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
                return Err(anyhow::anyhow!(
                    "Tailscale Funnel is not enabled for this tailnet.\n\n\
                     {detail}\n\n\
                     After enabling, re-run `aoe serve --remote`. \
                     Or pass --no-tailscale to use Cloudflare instead."
                ));
            }
            res = tokio::time::timeout(STEP_TIMEOUT, child.wait()) => {
                res
                    .map_err(|_| {
                        anyhow::anyhow!(
                            "tailscale funnel timed out after {}s; your node may not have \
                             HTTPS certs enabled or Funnel may not be enabled in your tailnet \
                             ACL. Run `AGENT_OF_EMPIRES_DEBUG=1 aoe serve --remote` and \
                             check debug.log for the live output, or try \
                             `tailscale funnel --bg --yes {}` manually, or pass \
                             --no-tailscale to skip.",
                            STEP_TIMEOUT.as_secs(),
                            local_port
                        )
                    })?
                    .map_err(|e| anyhow::anyhow!("failed to wait on tailscale funnel: {}", e))?
            }
        };

        info!(
            "`tailscale funnel` exited with status {:?} ({})",
            status.code(),
            if status.success() { "ok" } else { "error" }
        );
        if !status.success() {
            anyhow::bail!(
                "tailscale funnel exited with status {:?}. Confirm Funnel is enabled \
                 in your tailnet ACL (https://login.tailscale.com/admin/acls/file) \
                 and HTTPS is enabled on the node. Run with \
                 AGENT_OF_EMPIRES_DEBUG=1 to see the full stderr stream.",
                status.code()
            );
        }

        // Funnel is on now. If :443 had nothing on it before, everything there is this start's:
        // a start that fails or is cancelled from here turns it back off, so no Funnel is left
        // pointing at a port nothing serves. Anything else on :443 (the user's own Funnel or
        // Serve, a sibling path, or a state the pre-flight couldn't read) is never touched.
        let mut guard = FunnelStartGuard::new(program, preflight.unused_443);

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

/// True if `tailscale` is on PATH, the daemon is logged in, and Funnel is enabled for the
/// tailnet.
pub async fn tailscale_available() -> bool {
    // Cheapest possible check first.
    let version = Command::new("tailscale").arg("--version").output().await;
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
    let Ok(out) = version else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    // Then confirm the daemon is running and signed in.
    let status = Command::new("tailscale").arg("status").output().await;
    match &status {
        Ok(o) => debug!(
            exit = ?o.status.code(),
            success = o.status.success(),
            stderr = %String::from_utf8_lossy(&o.stderr).trim(),
            "tailscale_available: `tailscale status` returned"
        ),
        Err(e) => debug!(error = %e, "tailscale_available: `tailscale status` spawn failed"),
    }
    let Ok(out) = status else {
        return false;
    };
    out.status.success()
}

/// The `tailscale funnel` arguments that turn off the :443 Funnel. Without `--set-path` this
/// removes every path on :443, so it only runs when the pre-flight found :443 unused;
/// `tailscale funnel reset` would clear every port.
const FUNNEL_OFF_ARGS: [&str; 3] = ["funnel", "--https=443", "off"];
/// How long turning the Funnel back off may take.
const FUNNEL_OFF_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// Turns off the :443 Funnel a Tailscale start switched on, unless the start finishes.
/// The error path awaits [`Self::reset`]; `Drop` only covers a start that was cancelled.
struct FunnelStartGuard {
    program: std::ffi::OsString,
    armed: bool,
}

impl FunnelStartGuard {
    fn new(program: &std::ffi::OsStr, armed: bool) -> Self {
        Self {
            program: program.to_owned(),
            armed,
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
                "{error:#}. The Tailscale Funnel on :443 may still route `/` to this port: :443 \
                 had other config before this start, or its status couldn't be read, so it \
                 wasn't turned off; check `tailscale funnel status`"
            );
        }
        match self.reset().await {
            Ok(()) => error,
            Err(why) => anyhow::anyhow!(
                "{error:#}. Couldn't turn the Tailscale Funnel back off ({why}); run \
                 `tailscale funnel --https=443 off`"
            ),
        }
    }

    /// Turn the Funnel back off and wait for it, bounded; the Daemon may exit right after.
    async fn reset(&mut self) -> Result<(), String> {
        if !self.armed {
            return Ok(());
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
                info!("Turned off the Tailscale Funnel this start switched on");
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
        warn!(%why, "Couldn't turn off the Tailscale Funnel this start switched on");
        Err(why)
    }
}

impl Drop for FunnelStartGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // Cancelled mid-start: there's no task to await in, so block here, bounded and reaped
        // by `run_with_timeout`. A detached thread could die with the Daemon before it ran.
        let result = crate::process::run_with_timeout(
            std::process::Command::new(&self.program)
                .args(FUNNEL_OFF_ARGS)
                .stdin(std::process::Stdio::null()),
            FUNNEL_OFF_TIMEOUT,
        );
        match result {
            Ok(Some(out)) if out.status.success() => {
                info!("Turned off the Tailscale Funnel a cancelled start switched on")
            }
            other => warn!(
                ?other,
                "Couldn't turn off the Tailscale Funnel a cancelled start switched on"
            ),
        }
    }
}

/// What `tailscale funnel status --json` says about :443 before a start.
#[derive(Debug, Default, PartialEq, Eq)]
struct FunnelPreflight {
    /// A :443 handler this start must not replace.
    conflicting: Option<String>,
    /// The status was read and :443 had no Serve, Funnel or TCP config, so everything on it
    /// after `--bg` is this start's to turn off. False when the status couldn't be read.
    unused_443: bool,
}

/// Probe `tailscale funnel status --json` to see if port 443 is already configured.
async fn inspect_existing_funnel(program: &std::ffi::OsStr, local_port: u16) -> FunnelPreflight {
    let out = Command::new(program)
        .args(["funnel", "status", "--json"])
        .stdin(std::process::Stdio::null())
        .output()
        .await;
    let Ok(out) = out else {
        return FunnelPreflight::default();
    };
    if !out.status.success() {
        debug!(
            exit = ?out.status.code(),
            stderr = %String::from_utf8_lossy(&out.stderr).trim(),
            "inspect_existing_funnel: `tailscale funnel status --json` non-zero; state unknown"
        );
        return FunnelPreflight::default();
    }
    let parsed: serde_json::Value = match serde_json::from_slice(&out.stdout) {
        Ok(v) => v,
        Err(e) => {
            debug!(error = %e, "inspect_existing_funnel: JSON parse failed; state unknown");
            return FunnelPreflight::default();
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
    let mut preflight = FunnelPreflight {
        conflicting: None,
        unused_443: !keys("TCP")
            .chain(keys("Web"))
            .chain(keys("AllowFunnel"))
            .any(|k| on_443(k)),
    };
    // Walk the Web map looking for a port-443 handler whose target isn't something we can
    // safely replace.
    let Some(web) = parsed.get("Web").and_then(|w| w.as_object()) else {
        return preflight;
    };
    for (vhost, cfg) in web.iter() {
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
                preflight.conflicting = Some(format!("proxy {} -> {}", path, proxy));
                return preflight;
            } else if let Some(p) = handler.get("Path").and_then(|v| v.as_str()) {
                debug!(
                    vhost = %vhost,
                    path = %path,
                    file_path = %p,
                    "inspect_existing_funnel: port 443 held by file-server handler"
                );
                preflight.conflicting = Some(format!("file server {} -> {}", path, p));
                return preflight;
            } else if handler.get("Text").is_some() {
                debug!(
                    vhost = %vhost,
                    path = %path,
                    "inspect_existing_funnel: port 443 held by static-text handler"
                );
                preflight.conflicting = Some(format!("static text at {}", path));
                return preflight;
            } else {
                debug!(
                    vhost = %vhost,
                    path = %path,
                    handler = ?handler,
                    "inspect_existing_funnel: port 443 held by unknown handler type"
                );
                preflight.conflicting = Some(format!("unknown handler at {}", path));
                return preflight;
            }
        }
    }
    debug!(
        local_port = local_port,
        "inspect_existing_funnel: no conflicting :443 handler"
    );
    preflight
}

/// Verify this node has the Funnel node-capability set in its ACL.
async fn check_funnel_capability(program: &std::ffi::OsStr) -> anyhow::Result<()> {
    // Tailscale emits the cap key with the allowed-port list appended as a query string,
    // e.g. `?ports=443,8443,10000`.
    const FUNNEL_CAP_PREFIX: &str = "https://tailscale.com/cap/funnel-ports";
    let out = Command::new(program)
        .args(["status", "--json"])
        .stdin(std::process::Stdio::null())
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

/// Sync counterpart of `tailscale_available()` for use from the TUI (which avoids spinning
/// up a tokio runtime just to probe for a CLI).
pub fn tailscale_available_sync() -> bool {
    // Unlike the async path, this runs on the TUI render hot path, so we keep it cheap and
    // quiet on the happy path.
    let version = std::process::Command::new("tailscale")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    let version_ok = version.as_ref().map(|s| s.success()).unwrap_or(false);
    if !version_ok {
        debug!(
            error = ?version.as_ref().err(),
            exit = ?version.as_ref().ok().and_then(|s| s.code()),
            "tailscale_available_sync: `tailscale --version` failed"
        );
        return false;
    }
    let status = std::process::Command::new("tailscale")
        .arg("status")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    let status_ok = status.as_ref().map(|s| s.success()).unwrap_or(false);
    if !status_ok {
        debug!(
            error = ?status.as_ref().err(),
            exit = ?status.as_ref().ok().and_then(|s| s.code()),
            "tailscale_available_sync: `tailscale status` failed (likely not logged in)"
        );
    }
    status_ok
}

/// Sync counterpart of `check_funnel_capability()`.
pub fn tailscale_funnel_cap_ready_sync() -> bool {
    // Tailscale emits the cap key with the allowed-port list appended as a query string,
    // e.g. `?ports=443,8443,10000`.
    const FUNNEL_CAP_PREFIX: &str = "https://tailscale.com/cap/funnel-ports";
    let out = std::process::Command::new("tailscale")
        .args(["status", "--json"])
        .output();
    let Ok(out) = out else {
        debug!("tailscale_funnel_cap_ready_sync: spawn failed");
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
    fn funnel_preflight_tells_an_unused_443_from_a_conflict() {
        let proxy = |to: &str| serde_json::json!({"Web": {"node.ts.net:443": {"Handlers": {"/": {"Proxy": to}}}}});
        let unused = FunnelPreflight {
            conflicting: None,
            unused_443: true,
        };
        // (status document, expected): only a :443 with no config at all is this start's to undo.
        let cases = [
            (serde_json::json!({}), unused),
            // Other ports don't count.
            (
                serde_json::json!({"TCP": {"8443": {"HTTPS": true}}, "Web": {"node.ts.net:8443": {"Handlers": {"/": {"Proxy": "http://127.0.0.1:3000"}}}}}),
                FunnelPreflight {
                    conflicting: None,
                    unused_443: true,
                },
            ),
            // This port from an earlier start, or a stale loopback backend: replaced, not undone.
            (proxy("http://127.0.0.1:8081"), FunnelPreflight::default()),
            (proxy("http://127.0.0.1:9999"), FunnelPreflight::default()),
            // Private Serve with no Funnel yet still isn't this start's.
            (
                serde_json::json!({"TCP": {"443": {"HTTPS": true}}}),
                FunnelPreflight::default(),
            ),
            (
                serde_json::json!({"AllowFunnel": {"node.ts.net:443": true}}),
                FunnelPreflight::default(),
            ),
            (
                proxy("http://10.0.0.5:80"),
                FunnelPreflight {
                    conflicting: Some("proxy / -> http://10.0.0.5:80".into()),
                    unused_443: false,
                },
            ),
        ];
        for (doc, expected) in cases {
            assert_eq!(funnel_preflight(&doc, 8081), expected, "{doc}");
        }
    }

    /// A fake `tailscale` that logs each call's arguments. `status --json` answers the
    /// capability check first; the second call is the URL read, shaped by `url_read`.
    /// `funnel status --json` prints the `funnel-state` file (`None` makes it fail), and
    /// `funnel --https=443 off` runs `off` and then empties that state, as the real one does.
    #[cfg(unix)]
    fn fake_tailscale(
        dir: &std::path::Path,
        funnel_state: Option<&str>,
        url_read: &str,
        off: &str,
    ) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let fake = dir.join("tailscale");
        let log = dir.join("argv.log");
        let count = dir.join("status-count");
        let state = dir.join("funnel-state");
        let funnel_status = match funnel_state {
            Some(json) => {
                std::fs::write(&state, json).unwrap();
                format!("cat '{}'", state.display())
            }
            None => "exit 1".to_owned(),
        };
        let script = format!(
            r#"#!/bin/sh
echo "$*" >> '{log}'
case "$*" in
  "status --json")
    n=$(cat '{count}' 2>/dev/null || echo 0); n=$((n+1)); echo $n > '{count}'
    if [ "$n" = 1 ]; then
      echo '{{"Self":{{"CapMap":{{"https://tailscale.com/cap/funnel-ports?ports=443":[]}},"DNSName":"node.ts.net."}}}}'
      exit 0
    fi
    {url_read} ;;
  "funnel status --json") {funnel_status} ;;
  "funnel --https=443 off")
    {off}
    echo '{{}}' > '{state}' ;;
  *) exit 0 ;;
esac
"#,
            log = log.display(),
            count = count.display(),
            state = state.display(),
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
    async fn a_failed_tailscale_start_turns_off_only_the_funnel_it_switched_on() {
        const OFF: &str = "funnel --https=443 off";
        let ours = r#"{"TCP":{"443":{"HTTPS":true}},"Web":{"node.ts.net:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:8081"}}}},"AllowFunnel":{"node.ts.net:443":true}}"#;
        let serve_only = r#"{"TCP":{"443":{"HTTPS":true}},"Web":{"node.ts.net:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:8081"}}}}}"#;
        let sibling = r#"{"TCP":{"443":{"HTTPS":true}},"Web":{"node.ts.net:443":{"Handlers":{"/blog":{"Proxy":"http://127.0.0.1:3000"}}}},"AllowFunnel":{"node.ts.net:443":true}}"#;
        let ok_url = r#"echo '{"Self":{"DNSName":"node.ts.net."}}'"#;
        // (funnel state before the start, URL read, `off` body, start succeeds, Funnel turned
        // off, a phrase the error must hold). Every row but the first two keeps the prior state.
        let cases: [(Option<&str>, &str, &str, bool, bool, &str); 8] = [
            (Some("{}"), "exit 1", "", false, true, ""),
            // The reset itself fails, so the error says how to finish it.
            (
                Some("{}"),
                "exit 1",
                "exit 1",
                false,
                true,
                "run `tailscale funnel --https=443 off`",
            ),
            // The user's own Funnel, private Serve, or a sibling path is left alone.
            (
                Some(ours),
                "exit 1",
                "",
                false,
                false,
                "check `tailscale funnel status`",
            ),
            (
                Some(serve_only),
                "exit 1",
                "",
                false,
                false,
                "check `tailscale funnel status`",
            ),
            (
                Some(sibling),
                "exit 1",
                "",
                false,
                false,
                "check `tailscale funnel status`",
            ),
            // A status that couldn't be read or parsed doesn't prove :443 was unused.
            (
                None,
                "exit 1",
                "",
                false,
                false,
                "check `tailscale funnel status`",
            ),
            (
                Some("not json"),
                "exit 1",
                "",
                false,
                false,
                "check `tailscale funnel status`",
            ),
            (Some("{}"), ok_url, "", true, false, ""),
        ];
        for (state, url_read, off_body, succeeds, off, hint) in cases {
            let dir = tempfile::tempdir().unwrap();
            let fake = fake_tailscale(dir.path(), state, url_read, off_body);
            let result = TunnelHandle::spawn_tailscale_with(fake.as_os_str(), 8081).await;
            let row = format!("{state:?} / {url_read} / {off_body}");
            assert_eq!(result.is_ok(), succeeds, "{row}");
            if let Err(e) = &result {
                assert!(e.to_string().contains(hint), "{row}: {e}");
            }
            // Awaited before the error returns, not left to a thread that may die with the Daemon.
            let calls = argv_log(dir.path());
            assert_eq!(
                calls.iter().filter(|c| *c == OFF).count(),
                usize::from(off),
                "{row}: {calls:?}"
            );
            let after = std::fs::read_to_string(dir.path().join("funnel-state")).ok();
            if !off {
                assert_eq!(after.as_deref(), state, "{row}");
            }
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
            let (url_read, off) = if blocked_in == "url read" {
                (format!("{block}; exit 1"), String::new())
            } else {
                let once = dir.path().join("off-once");
                let once = once.display();
                let off = format!("if [ ! -f '{once}' ]; then touch '{once}'; {block}; fi");
                ("exit 1".to_owned(), off)
            };
            let fake = fake_tailscale(dir.path(), Some("{}"), &url_read, &off);
            let program = fake.clone();
            let start = tokio::spawn(async move {
                TunnelHandle::spawn_tailscale_with(program.as_os_str(), 8081).await
            });

            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            let before = loop {
                let calls = argv_log(dir.path());
                let ready = if blocked_in == "url read" {
                    count(&calls, "status --json") >= 2
                } else {
                    count(&calls, OFF) >= 1
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
            std::fs::write(&release, "").unwrap();
            assert_eq!(count(&after, OFF), offs, "{blocked_in}: {after:?}");
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
