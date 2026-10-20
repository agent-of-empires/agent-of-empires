//! Start the shared local daemon when a client needs one and none answers.
//!
//! Clients serialize on `serve.autostart.lock` and re-probe once they hold it,
//! so of several racing clients exactly one spawns and the rest connect to it.
//! The spawn itself is `aoe serve --daemon`, which keeps its own
//! `serve.state.lock` and pid checks against a concurrent manual start.

use std::path::Path;
use std::time::{Duration, Instant};

use fs2::FileExt as _;
use rand::RngExt;

use super::{discovery::discover_local, DaemonEndpoint, HttpClient};
use crate::cli::serve::daemon_pid;

const LOG_TAIL_LINES: usize = 200;
/// A warm start takes about 1s; this covers a cold start without hanging a caller forever.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(20);
const POLL_INTERVAL: Duration = Duration::from_millis(300);

/// The local daemon, if one is alive and answers a health check.
pub(crate) async fn healthy_local() -> Option<DaemonEndpoint> {
    let endpoint = discover_local().ok()?;
    let client = HttpClient::new(endpoint.clone()).ok()?;
    client.health_check().await.ok()?;
    Some(endpoint)
}

/// Spawn the daemon unless a racing client already did. The pid is `Some` only
/// for the caller that spawned it.
pub(crate) async fn start_local() -> Result<(DaemonEndpoint, Option<u32>), String> {
    let _lock = tokio::task::spawn_blocking(lock_autostart)
        .await
        .map_err(|e| format!("lock task failed: {e}"))??;
    if let Some(endpoint) = healthy_local().await {
        return Ok((endpoint, None));
    }

    let exe = std::env::current_exe().map_err(|e| format!("could not resolve aoe binary: {e}"))?;
    let port = load_or_generate_port();
    // The wrapper double-forks; the daemon's output lands in the log file.
    let status = tokio::process::Command::new(exe)
        .args(["serve", "--daemon", "--host", "127.0.0.1", "--port"])
        .arg(port.to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map_err(|e| format!("failed to launch `aoe serve --daemon`: {e}"))?;
    // A non-zero exit with a live daemon means a manual start won the race.
    let spawned = status.success();
    if !spawned && daemon_pid().is_none() {
        return Err(spawn_failure(status.code(), port));
    }

    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        if let Some(endpoint) = healthy_local().await {
            return Ok((endpoint, daemon_pid().filter(|_| spawned)));
        }
        if Instant::now() >= deadline {
            // The detached child binds after the wrapper exits 0, so a port clash lands here.
            forget_port_if_in_use();
            return Err(format!(
                "the daemon was started but never became reachable.{}",
                log_hint()
            ));
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

fn lock_autostart() -> Result<std::fs::File, String> {
    let path = crate::session::get_app_dir()
        .map_err(|e| format!("could not resolve the app directory: {e}"))?
        .join("serve.autostart.lock");
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|e| format!("could not open {}: {e}", path.display()))?;
    file.lock_exclusive()
        .map_err(|e| format!("could not lock {}: {e}", path.display()))?;
    Ok(file)
}

fn spawn_failure(code: Option<i32>, port: u16) -> String {
    forget_port_if_in_use();
    format!(
        "`aoe serve --daemon` exited with {code:?}. Most likely port {port} is in use.{}",
        log_hint()
    )
}

/// Forget an occupied port so the next attempt picks a fresh one.
fn forget_port_if_in_use() {
    let tail = initial_log_tail().join("\n");
    if tail.contains("EADDRINUSE") || tail.contains("Address already in use") {
        if let Ok(dir) = crate::session::get_app_dir() {
            let _ = std::fs::remove_file(dir.join("serve.last_port"));
        }
    }
}

fn log_hint() -> String {
    let tail = initial_log_tail();
    if tail.is_empty() {
        String::new()
    } else {
        format!(" Last log lines:\n{}", tail.join("\n"))
    }
}

pub(crate) fn load_or_generate_port() -> u16 {
    match crate::session::get_app_dir() {
        Ok(dir) => load_or_generate_port_in(&dir),
        Err(_) => random_port(),
    }
}

fn load_or_generate_port_in(dir: &Path) -> u16 {
    let path = dir.join("serve.last_port");
    let saved = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| raw.trim().parse::<u16>().ok())
        .filter(|port| *port >= 49152);
    saved.unwrap_or_else(|| {
        let port = random_port();
        let _ = std::fs::write(&path, port.to_string());
        port
    })
}

fn random_port() -> u16 {
    rand::rng().random_range(49152..65535)
}

/// The current run's lines: the log is shared, so start at the last `[AOE_START_MARKER]`.
pub(crate) fn initial_log_tail() -> Vec<String> {
    let Some(contents) = crate::cli::serve::stdio_redirect_path()
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
    else {
        return Vec::new();
    };
    let all: Vec<&str> = contents.lines().collect();
    let from = all
        .iter()
        .rposition(|line| line.contains("[AOE_START_MARKER]"))
        .unwrap_or(0);
    let window = &all[from..];
    let start = window.len().saturating_sub(LOG_TAIL_LINES);
    window[start..].iter().map(|s| s.to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_is_reused_only_when_valid() {
        for (saved, reused) in [
            (None, None),
            (Some("55555"), Some(55555)),
            (Some("8080"), None),
            (Some("not-a-number\n"), None),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("serve.last_port");
            if let Some(saved) = saved {
                std::fs::write(&path, saved).unwrap();
            }
            let port = load_or_generate_port_in(tmp.path());
            match reused {
                Some(expected) => assert_eq!(port, expected),
                None => {
                    assert!(port >= 49152, "{saved:?} gave {port}");
                    assert_eq!(std::fs::read_to_string(&path).unwrap(), port.to_string());
                }
            }
        }
    }
}
