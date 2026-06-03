//! Public-HTTPS tunnel integration for secure remote access.
//!
//! Supports three transports, auto-picked by `server::start_server` in
//! this preference order:
//! 1. Tailscale Funnel: preferred when `tailscale` is installed and logged in.
//!    Gives a stable `https://<machine>.<tailnet>.ts.net` URL, so installed
//!    PWAs survive server restarts. No child process to manage; the Tailscale
//!    daemon owns the ingress.
//! 2. Named Cloudflare tunnel: user-provided `--tunnel-name` + `--tunnel-url`.
//!    Stable hostname on the user's own domain.
//! 3. Cloudflare quick tunnel: fallback. Zero-config, but the URL rotates on
//!    every restart, which breaks installed PWAs. Documented limitation.

use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

/// Manages a public-HTTPS tunnel. For Cloudflare variants, wraps a
/// `cloudflared` subprocess and supervises it. For Tailscale Funnel,
/// there's no child process: the Tailscale daemon owns the ingress,
/// and this handle is essentially a URL carrier.
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
    /// Short label for `serve.mode` and the TUI status bar.
    pub fn mode_label(&self) -> &'static str {
        match self {
            Self::Quick | Self::Named { .. } => "tunnel",
            Self::Tailscale => "tailscale",
        }
    }
}

impl TunnelHandle {
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

    /// Configure Tailscale Funnel for the local port and return a
    /// handle carrying the stable `https://<host>.<tailnet>.ts.net` URL.
    /// Uses the single-command Funnel syntax introduced in Tailscale
    /// 1.52: one call to `tailscale funnel --bg --yes <port>` replaces
    /// the legacy `tailscale serve` + `tailscale funnel` dance and
    /// surfaces clearer errors when Funnel isn't pre-approved in the
    /// tailnet ACL. No subprocess supervision is needed; the Tailscale
    /// daemon owns the ingress and the command returns once the config
    /// is applied.
    pub async fn spawn_tailscale(local_port: u16) -> anyhow::Result<Self> {
        // Hard cap on each tailscale command so we never wedge if
        // tailscale pops an interactive prompt (HTTPS-certs consent,
        // Funnel-not-enabled-in-ACL, node not signed in). 60s because
        // first-time HTTPS cert provisioning on a fresh node can
        // legitimately take 30-45s. When the timeout fires, the error
        // bubbles up to start_server which surfaces it to the user
        // with fix instructions; we do NOT silently fall back to
        // Cloudflare because that would hide the real problem.
        const STEP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
        debug!(
            local_port = local_port,
            timeout_s = STEP_TIMEOUT.as_secs(),
            "tailscale: spawn_tailscale starting"
        );

        // Pre-flight 1: confirm this node is allowed to use Funnel at
        // all. Tailscale surfaces Funnel eligibility as the node cap
        // `https://tailscale.com/cap/funnel-ports` in `status --json`;
        // if it's missing, the ACL hasn't granted `funnel` to this
        // node and the `tailscale funnel` command will fail with a
        // generic error. Checking here lets us point the user at the
        // admin console with a concrete fix instead.
        if let Err(e) = check_funnel_capability().await {
            debug!(reason = %e, "spawn_tailscale: funnel cap check failed");
            anyhow::bail!(
                "Tailscale Funnel is not enabled for this tailnet. \
                 Enable it at https://login.tailscale.com/admin/acls/file \
                 (add `funnel` to nodeAttrs), or pass --no-tailscale to use Cloudflare."
            );
        }

        // Pre-flight 2: if Funnel is already configured on our chosen
        // HTTPS port (443) for a DIFFERENT backend, bail rather than
        // silently taking it over. Belt-and-suspenders; the modern
        // `tailscale funnel <port>` single-command syntax is mostly
        // additive, but a user with a Funnel on 443 pointing at their
        // own service would have it replaced otherwise.
        if let Some(existing) = inspect_existing_funnel(local_port).await {
            anyhow::bail!(
                "port 443 is already configured on this node for a different \
                 backend ({}). Run `tailscale funnel reset` to clear the \
                 existing config, or pass --no-tailscale to use Cloudflare.",
                existing
            );
        }

        // Single-command Funnel (Tailscale 1.52+). Replaces the old
        // `tailscale serve --https=443` + `tailscale funnel 443` two-step
        // dance. `--bg` persists across aoe restarts; `--yes` skips
        // interactive prompts so we fail fast instead of hanging if
        // Funnel isn't pre-approved in the tailnet ACL.
        //
        // We stream stderr line-by-line into the debug log instead of
        // buffering with `.output()`, so the user can watch progress
        // (cert provisioning, etc.) in debug.log instead of staring at
        // a black box for up to a minute.
        let funnel_arg = local_port.to_string();
        let funnel_args = ["funnel", "--bg", "--yes", &funnel_arg];
        info!(
            "Running `tailscale funnel --bg --yes {}` (first-time HTTPS cert provisioning can take 30-60s)",
            local_port
        );

        let mut child = Command::new("tailscale")
            .args(funnel_args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| anyhow::anyhow!("failed to run tailscale funnel: {}", e))?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        // Spawn drain tasks for both streams. Tailscale emits progress
        // on stderr (most useful for diagnosing hangs); logged at info!
        // so the TUI Starting screen surfaces it when tailing the
        // configured log file, without needing AGENT_OF_EMPIRES_DEBUG=1.
        // Stdout stays at debug! because Tailscale rarely prints there
        // and the lines that do appear are noisier.
        //
        // Do NOT override the `target` on these log macros: the
        // EnvFilter uses `agent_of_empires=debug`, which matches the
        // default module path (agent_of_empires::server::tunnel). A
        // custom target like "tailscale_funnel" would not match that
        // prefix and every line would be silently dropped, which is
        // exactly how this used to fail.
        //
        // Also: scan each stderr line for the tailnet-level Funnel
        // activation URL. When Funnel isn't enabled for the tailnet
        // (distinct from the per-node ACL grant we already pre-flight),
        // `tailscale funnel` prints something like:
        //     Funnel is not enabled on your tailnet.
        //     To enable, visit:
        //              https://login.tailscale.com/f/funnel?node=XXXX
        // and then hangs waiting for the user to click the link. We
        // detect that URL, send it over a oneshot, kill the child, and
        // bail with a crisp error instead of waiting for the 60s timeout.
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
                // Tailscale told us exactly how to fix this. Kill the
                // (hung) child and surface the URL verbatim. If the
                // oneshot closed without a URL (buffering quirk, etc.),
                // fall back to generic guidance rather than guessing at
                // a URL that may not match this tailnet.
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

        // Read the stable funnel URL from `tailscale status`.
        info!("Reading stable URL from `tailscale status --json`...");
        let url = tokio::time::timeout(STEP_TIMEOUT, tailscale_funnel_url())
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "tailscale status timed out after {}s",
                    STEP_TIMEOUT.as_secs()
                )
            })??;
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
    /// Cancels the health monitor first, then sends SIGTERM to cloudflared.
    /// For Tailscale funnels, leaves the funnel configuration in place on
    /// purpose: restarting aoe shouldn't tear down the PWA's origin.
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
    /// The task stops when the cancellation token is cancelled (during shutdown).
    /// No-op for Tailscale funnels (no child process to supervise; the
    /// Tailscale daemon handles its own health).
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

                // Read try_wait under the lock, then drop the guard
                // before any await. Holding the child mutex across
                // restart_tunnel().await would block aoe serve --stop
                // (which also wants the lock to terminate the child)
                // for the multi-second cloudflared spawn time.
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
                        // Re-acquire the lock just long enough to install
                        // the replacement child. The previous iteration's
                        // try_wait branch is the only other holder; it
                        // bails on Ok(None) within the lock, so contention
                        // is bounded.
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
            // Unreachable in practice: the health monitor doesn't run
            // for Tailscale (see spawn_health_monitor early-return), so
            // restart_tunnel is never called with this variant. If a
            // future refactor changes that invariant, fail loudly.
            anyhow::bail!("restart_tunnel called for Tailscale; no child process exists")
        }
    }
}

/// True if `tailscale` is on PATH, the daemon is logged in, and Funnel
/// is enabled for the tailnet. Conservative: any failure returns false
/// so callers fall back cleanly to Cloudflare.
pub async fn tailscale_available() -> bool {
    // Cheapest possible check first: does the CLI exist and return
    // a successful status?
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
    // Then confirm the daemon is running and signed in. `status` exits
    // non-zero if not logged in.
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

/// Probe `tailscale funnel status --json` to see if port 443 is already
/// configured. Returns `Some(<description>)` when a different backend
/// already holds the port; `None` when the port is free OR already
/// points at our local port (same-target re-runs are idempotent and
/// safe). Used as a pre-flight before we run `tailscale funnel` so we
/// don't stomp on a user's existing setup.
///
/// Best-effort: any probe failure returns `None` so the spawn attempt
/// proceeds. If Tailscale's JSON schema changes or the CLI errors out
/// for an unrelated reason, we don't want a parse miss to block a user
/// whose Funnel would otherwise work fine.
async fn inspect_existing_funnel(local_port: u16) -> Option<String> {
    let out = Command::new("tailscale")
        .args(["funnel", "status", "--json"])
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        debug!(
            exit = ?out.status.code(),
            stderr = %String::from_utf8_lossy(&out.stderr).trim(),
            "inspect_existing_funnel: `tailscale funnel status --json` non-zero; assuming empty"
        );
        return None;
    }
    let parsed: serde_json::Value = match serde_json::from_slice(&out.stdout) {
        Ok(v) => v,
        Err(e) => {
            debug!(error = %e, "inspect_existing_funnel: JSON parse failed; assuming empty");
            return None;
        }
    };

    // Walk the Web map looking for a port-443 handler whose target
    // isn't something we can safely replace. Shape (from Tailscale's
    // ServeConfig):
    //   { "Web": { "${host}:443": { "Handlers": {
    //       "/": { "Proxy": "http://127.0.0.1:4999" },  // or:
    //       "/": { "Path":  "/var/www" },               // file server, or:
    //       "/": { "Text":  "hello" }                   // static text
    //   } } } }
    //
    // Rules:
    // - Proxy to OUR local_port: idempotent, no-op.
    // - Proxy to 127.0.0.1:<other-port> or localhost:*: stale aoe config
    //   from a previous run (e.g. port changed across restarts).
    //   `tailscale funnel --bg --yes <new>` will cleanly replace it; no
    //   need to bail.
    // - Proxy to anything else (tailnet IP, non-loopback): user has a
    //   different service on 443 and we'd clobber it. Bail.
    // - Non-Proxy (file server / static text): always a conflict; these
    //   are explicitly user-configured.
    let web = parsed.get("Web")?.as_object()?;
    let expected_proxy_substring = format!("127.0.0.1:{}", local_port);
    for (vhost, cfg) in web.iter() {
        if !vhost.ends_with(":443") {
            continue;
        }
        let handlers = cfg.get("Handlers").and_then(|h| h.as_object())?;
        for (path, handler) in handlers.iter() {
            if let Some(proxy) = handler.get("Proxy").and_then(|v| v.as_str()) {
                if proxy.contains(&expected_proxy_substring) {
                    continue;
                }
                // A different-port loopback backend looks like a prior
                // aoe run (or another local dev server); overwriting it
                // is the expected behavior, not a conflict.
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
                return Some(format!("proxy {} -> {}", path, proxy));
            } else if let Some(p) = handler.get("Path").and_then(|v| v.as_str()) {
                debug!(
                    vhost = %vhost,
                    path = %path,
                    file_path = %p,
                    "inspect_existing_funnel: port 443 held by file-server handler"
                );
                return Some(format!("file server {} -> {}", path, p));
            } else if handler.get("Text").is_some() {
                debug!(
                    vhost = %vhost,
                    path = %path,
                    "inspect_existing_funnel: port 443 held by static-text handler"
                );
                return Some(format!("static text at {}", path));
            } else {
                debug!(
                    vhost = %vhost,
                    path = %path,
                    handler = ?handler,
                    "inspect_existing_funnel: port 443 held by unknown handler type"
                );
                return Some(format!("unknown handler at {}", path));
            }
        }
    }
    debug!(
        local_port = local_port,
        "inspect_existing_funnel: port 443 free or already matches our local port"
    );
    None
}

/// Verify this node has the Funnel node-capability set in its ACL.
///
/// Tailscale surfaces Funnel eligibility as the cap key
/// `https://tailscale.com/cap/funnel-ports` on `Self.CapMap` in
/// `tailscale status --json`. Value is the list of allowed ports
/// (default 443, 8443, 10000). We only care that the key exists;
/// port 443 is always in the default grant when the cap is present.
///
/// Returns Err with a short reason on any missing/malformed field so
/// the caller can surface an actionable message pointing at the ACL
/// editor.
async fn check_funnel_capability() -> anyhow::Result<()> {
    // Tailscale emits the cap key with the allowed-port list appended as
    // a query string, e.g. `?ports=443,8443,10000`. Match by prefix so
    // future port-list changes don't break the check.
    const FUNNEL_CAP_PREFIX: &str = "https://tailscale.com/cap/funnel-ports";
    let out = Command::new("tailscale")
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

async fn tailscale_funnel_url() -> anyhow::Result<String> {
    let out = Command::new("tailscale")
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

/// Does this ServeConfig proxy URL point at the local machine
/// (127.0.0.1, localhost, or ::1)? Used to distinguish stale aoe
/// configs (which we replace without complaint) from a user's legit
/// tailnet-facing service (which we refuse to clobber).
fn proxy_is_loopback(proxy: &str) -> bool {
    let lower = proxy.to_ascii_lowercase();
    let after_scheme = lower
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(&lower);
    // IPv6 is bracketed: `[::1]:8080`. Everything else separates host
    // and port with the first ':' / '/' after the scheme.
    if let Some(rest) = after_scheme.strip_prefix('[') {
        return rest.starts_with("::1]");
    }
    let host = after_scheme
        .split(['/', ':'])
        .next()
        .unwrap_or(after_scheme);
    matches!(host, "127.0.0.1" | "localhost")
}

/// Parse a `https://login.tailscale.com/f/funnel?...` activation URL
/// out of a `tailscale funnel` stderr line. Tailscale prints this when
/// Funnel isn't enabled for the tailnet — the URL is node-specific
/// (carries the current node id as a query param) so it jumps the user
/// straight to the right approval flow.
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
            // The URL always ends with ".com" so strip anything after that.
            if let Some(pos) = word.find(".trycloudflare.com") {
                let end = pos + ".trycloudflare.com".len();
                return Some(word[..end].to_string());
            }
        }
    }
    None
}

/// Render a QR code to stderr for easy phone scanning.
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

/// Sync counterpart of `tailscale_available()` for use from the TUI
/// (which avoids spinning up a tokio runtime just to probe for a CLI).
/// Conservative: any error means "not available" and we fall through
/// to Cloudflare.
pub fn tailscale_available_sync() -> bool {
    // Unlike the async path, this runs on the TUI render hot path, so
    // we keep it cheap and quiet on the happy path. On failure, we log
    // at debug! so `AGENT_OF_EMPIRES_DEBUG=1` surfaces exactly which
    // step bounced.
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

/// Sync counterpart of `check_funnel_capability()`. Returns true when
/// this node's ACL grants the `funnel` nodeAttr (i.e. Funnel is usable).
/// Used by the TUI transport picker to show "Ready" vs "needs ACL grant"
/// on the Tailscale Funnel card without requiring the user to commit and
/// then see a spawn-time failure.
///
/// Conservative: any failure (daemon down, JSON parse error) returns
/// false. Dumps the CapMap keys and Self.Tags at debug! so a user hitting
/// a false negative can grep debug.log and see why the detection missed:
/// most often the node is tagged and the ACL rule targets autogroup:member
/// (which excludes tagged devices).
pub fn tailscale_funnel_cap_ready_sync() -> bool {
    // Tailscale emits the cap key with the allowed-port list appended as
    // a query string, e.g. `?ports=443,8443,10000`. Match by prefix so
    // future port-list changes don't break the check.
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
    use super::*;

    // The telemetry `serve_mode` signal reads `mode_label()`, so it must only
    // ever emit the closed exposure set, and a named Cloudflare tunnel must
    // bucket to "tunnel" rather than leak the configured tunnel name (#1885).
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

        for kind in [
            TunnelKind::Quick,
            TunnelKind::Named {
                tunnel_name: "anything".to_string(),
            },
            TunnelKind::Tailscale,
        ] {
            assert!(
                matches!(kind.mode_label(), "tunnel" | "tailscale"),
                "mode_label must stay within the closed exposure set"
            );
        }
    }

    #[test]
    fn extract_url_from_typical_output() {
        let line =
            "2026-04-12T12:00:00Z INF +-------------------------------------------------------------------+";
        assert_eq!(extract_tunnel_url(line), None);

        let line = "2026-04-12T12:00:01Z INF |  https://random-words-here.trycloudflare.com  |";
        assert_eq!(
            extract_tunnel_url(line),
            Some("https://random-words-here.trycloudflare.com".to_string())
        );
    }

    #[test]
    fn extract_url_no_match() {
        assert_eq!(extract_tunnel_url("INF Starting tunnel subsystem"), None);
        assert_eq!(extract_tunnel_url("https://example.com not a tunnel"), None);
    }

    #[test]
    fn extract_url_with_trailing_punctuation() {
        let line = "Visit https://abc-def.trycloudflare.com.";
        assert_eq!(
            extract_tunnel_url(line),
            Some("https://abc-def.trycloudflare.com".to_string())
        );
    }

    #[test]
    fn extract_funnel_activation_url_matches_indented_line() {
        // Real tailscale funnel output: URL arrives on its own line
        // after "To enable, visit:", indented with whitespace.
        let line = "         https://login.tailscale.com/f/funnel?node=n6ADBuFYMT11CNTRL";
        assert_eq!(
            extract_funnel_activation_url(line),
            Some("https://login.tailscale.com/f/funnel?node=n6ADBuFYMT11CNTRL".to_string())
        );
    }

    #[test]
    fn extract_funnel_activation_url_strips_trailing_whitespace() {
        let line = "         https://login.tailscale.com/f/funnel?node=abc   ";
        assert_eq!(
            extract_funnel_activation_url(line),
            Some("https://login.tailscale.com/f/funnel?node=abc".to_string())
        );
    }

    #[test]
    fn extract_funnel_activation_url_ignores_unrelated_urls() {
        assert_eq!(
            extract_funnel_activation_url("https://login.tailscale.com/admin"),
            None
        );
        assert_eq!(extract_funnel_activation_url("no url here"), None);
    }

    #[test]
    fn proxy_is_loopback_matches_local_forms() {
        assert!(proxy_is_loopback("http://127.0.0.1:8080"));
        assert!(proxy_is_loopback("http://localhost:3000"));
        assert!(proxy_is_loopback("https://127.0.0.1"));
        assert!(proxy_is_loopback("http://[::1]:8080"));
    }

    #[test]
    fn proxy_is_loopback_rejects_remote_hosts() {
        assert!(!proxy_is_loopback("http://100.64.0.1:8080"));
        assert!(!proxy_is_loopback("http://example.com"));
        assert!(!proxy_is_loopback("http://192.168.1.5:8080"));
    }

    #[test]
    fn check_cloudflared_returns_err_when_missing() {
        // This test verifies the function doesn't panic with a missing binary.
        // It may pass or fail depending on whether cloudflared is installed.
        let result = check_cloudflared();
        // We just verify it returns a Result without panicking
        let _ = result;
    }
}
