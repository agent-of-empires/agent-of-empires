//! OpenCode session id preassignment through a short-lived `opencode serve`.

use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use uuid::Uuid;

/// Budget for serve boot plus the create POST (boot measured at ~1.8s).
const OPENCODE_PREASSIGN_DEADLINE: Duration = Duration::from_secs(6);

/// Reaps the ephemeral `opencode serve` process group on drop, so no attempt
/// leaks a server and two servers never share the SQLite store.
struct ServeGuard(Option<std::process::Child>);

impl Drop for ServeGuard {
    fn drop(&mut self) {
        let Some(mut child) = self.0.take() else {
            return;
        };
        signal_serve_group(child.id(), Signal::Term);
        std::thread::sleep(Duration::from_millis(150));
        if matches!(child.try_wait(), Ok(None)) {
            signal_serve_group(child.id(), Signal::Kill);
        }
        let _ = child.wait();
    }
}

enum Signal {
    Term,
    Kill,
}

/// Signals the process group, then the bare pid. No-op off unix.
fn signal_serve_group(pid: u32, signal: Signal) {
    #[cfg(unix)]
    {
        use nix::sys::signal::{kill, killpg, Signal as Nix};
        let sig = match signal {
            Signal::Term => Nix::SIGTERM,
            Signal::Kill => Nix::SIGKILL,
        };
        let p = nix::unistd::Pid::from_raw(pid as i32);
        let _ = killpg(p, sig);
        let _ = kill(p, sig);
    }
    #[cfg(not(unix))]
    let _ = (pid, signal);
}

/// Creates the OpenCode session up front under the launch's own `environment`
/// so both processes select the same store. Any failure leaves it unowned.
pub(crate) fn preassign_opencode_session_id(
    project_path: &str,
    command: std::process::Command,
) -> Option<String> {
    preassign(project_path, command)
        .map_err(|e| {
            tracing::warn!(
                target: "session.capture",
                "opencode session preassign failed ({e}); automatic capture remains disabled"
            )
        })
        .ok()
        .and_then(super::validated_session_id)
}

fn preassign(project_path: &str, mut cmd: std::process::Command) -> Result<String> {
    // The bind/drop/bind race is covered by the readiness deadline.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .context("failed to reserve a loopback port for opencode serve")?
        .local_addr()
        .context("failed to read the reserved loopback port")?
        .port();
    let id = format!("ses_{}", Uuid::new_v4().simple());

    cmd.args([
        "serve",
        "--hostname",
        "127.0.0.1",
        "--port",
        &port.to_string(),
    ])
    .current_dir(project_path)
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let child = cmd
        .spawn()
        .context("failed to spawn `opencode serve` for preassign")?;
    let _guard = ServeGuard(Some(child));

    let base = format!("http://127.0.0.1:{port}");
    // The caller may already be inside a Tokio runtime, so the blocking runtime
    // runs on its own OS thread.
    let run = || -> Result<()> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("failed to build the preassign runtime")?;
        rt.block_on(create_session(&base, &id, project_path))
    };
    std::thread::scope(|scope| {
        scope
            .spawn(run)
            .join()
            .map_err(|_| anyhow::anyhow!("opencode preassign worker thread panicked"))?
    })?;
    Ok(id)
}

async fn create_session(base: &str, id: &str, project_path: &str) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .context("failed to build the preassign HTTP client")?;

    let deadline = Instant::now() + OPENCODE_PREASSIGN_DEADLINE;
    loop {
        if let Ok(resp) = client.get(format!("{base}/api/session")).send().await {
            if resp.status().is_success() {
                break;
            }
        }
        if Instant::now() >= deadline {
            anyhow::bail!("opencode serve did not become ready within the deadline");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let body = serde_json::json!({
        "id": id,
        "location": { "directory": project_path },
    });
    let resp = client
        .post(format!("{base}/api/session"))
        .json(&body)
        .send()
        .await
        .context("opencode preassign POST /api/session failed")?;
    if !resp.status().is_success() {
        anyhow::bail!("opencode preassign POST returned {}", resp.status());
    }
    let created: serde_json::Value = resp
        .json()
        .await
        .context("opencode preassign response was not JSON")?;
    let created_id = created
        .get("data")
        .and_then(|d| d.get("id"))
        .and_then(|v| v.as_str());
    if created_id != Some(id) {
        anyhow::bail!("opencode assigned {created_id:?}, expected {id}");
    }
    Ok(())
}
