//! Daemon process control and the `$APP_DIR/serve.*` files the view reads and writes.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;

use rand::prelude::IndexedRandom;
use rand::RngExt;

use super::words::PASSPHRASE_WORDS;
use super::{ServeMode, TransportStatus, TunnelTransport};

const LOG_TAIL_LINES: usize = 200;
const SAVED_PASSPHRASE_FILE: &str = "serve.saved_passphrase";
/// Written by the server at startup, so CLI-launched daemons can show their passphrase.
const EPHEMERAL_PASSPHRASE_FILE: &str = "serve.passphrase";

/// Passphrase of the daemon this process spawned, cleared when it stops it.
static LAST_SPAWNED_PASSPHRASE: Mutex<Option<String>> = Mutex::new(None);

fn app_file(name: &str) -> Option<PathBuf> {
    crate::session::get_app_dir().ok().map(|dir| dir.join(name))
}

fn read_app_file(name: &str) -> Option<String> {
    let raw = std::fs::read_to_string(app_file(name)?).ok()?;
    let trimmed = raw.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn remove_app_file(name: &str) {
    if let Some(path) = app_file(name) {
        let _ = std::fs::remove_file(path);
    }
}

fn set_cached_passphrase(pp: Option<&str>) {
    if let Ok(mut guard) = LAST_SPAWNED_PASSPHRASE.lock() {
        *guard = pp.map(str::to_string);
    }
}

fn cached_passphrase() -> Option<String> {
    LAST_SPAWNED_PASSPHRASE.lock().ok()?.clone()
}

pub(super) fn remember_passphrase(pp: &str) {
    set_cached_passphrase(Some(pp));
}

pub(super) fn recall_passphrase() -> Option<String> {
    if let Some(pp) = cached_passphrase() {
        tracing::debug!(target: "tui.dialog", "passphrase recalled from in-memory cache");
        return Some(pp);
    }
    if let Some(pp) = read_app_file(SAVED_PASSPHRASE_FILE) {
        tracing::debug!(target: "tui.dialog", "passphrase recalled from serve.saved_passphrase");
        return Some(pp);
    }
    let pp = read_app_file(EPHEMERAL_PASSPHRASE_FILE)?;
    tracing::debug!(target: "tui.dialog", "passphrase recalled from serve.passphrase on disk");
    Some(pp)
}

/// Persist the passphrase that survives stop/start, owner-readable only.
pub(super) fn save_passphrase_to_disk(pp: &str) {
    let Some(path) = app_file(SAVED_PASSPHRASE_FILE) else {
        return;
    };
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
        {
            let _ = file.write_all(pp.as_bytes());
        }
    }
    #[cfg(not(unix))]
    {
        let _ = std::fs::write(path, pp);
    }
}

pub(super) fn load_or_generate_passphrase() -> String {
    read_app_file(SAVED_PASSPHRASE_FILE).unwrap_or_else(|| {
        let pp = generate_passphrase();
        save_passphrase_to_disk(&pp);
        pp
    })
}

/// Four words from the list: about 35 bits, enough as a second factor and easy to type on a phone.
pub(super) fn generate_passphrase() -> String {
    let mut rng = rand::rng();
    let words: Vec<&str> = (0..4)
        .map(|_| {
            *PASSPHRASE_WORDS
                .choose(&mut rng)
                .expect("wordlist nonempty")
        })
        .collect();
    words.join(" ")
}

pub(super) fn assess_transports() -> (TransportStatus, TransportStatus) {
    let tailscale = if !crate::server::tunnel::tailscale_available_sync() {
        TransportStatus::NotInstalled
    } else if !crate::server::tunnel::tailscale_funnel_cap_ready_sync() {
        TransportStatus::FunnelNotEnabled
    } else {
        TransportStatus::Ready
    };
    let cloudflare = if crate::server::tunnel::check_cloudflared().is_ok() {
        TransportStatus::Ready
    } else {
        TransportStatus::NotInstalled
    };
    (tailscale, cloudflare)
}

/// A ready Tailscale wins (stable URL); with neither ready, Tailscale shows the fix instructions.
pub(super) fn default_transport(
    tailscale: TransportStatus,
    cloudflare: TransportStatus,
) -> TunnelTransport {
    match (tailscale, cloudflare) {
        (TransportStatus::Ready, _) => TunnelTransport::Tailscale,
        (_, TransportStatus::Ready) => TunnelTransport::Cloudflare,
        _ => TunnelTransport::Tailscale,
    }
}

/// Spawn a Local daemon and wait until discovery finds it and it answers a health check.
pub(crate) async fn start_local_daemon_and_wait(
) -> Result<crate::acp::client::DaemonEndpoint, String> {
    use crate::acp::client::{discovery::discover, HttpClient};

    spawn_daemon(ServeMode::Local, None, None)?;
    // A warm start takes about 1s; 20s covers a cold start without wedging the UI.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        if let Ok(endpoint) = discover() {
            if let Ok(client) = HttpClient::new(endpoint.clone()) {
                if client.health_check().await.is_ok() {
                    return Ok(endpoint);
                }
            }
        }
        if std::time::Instant::now() >= deadline {
            let tail = initial_log_tail();
            let hint = if tail.is_empty() {
                String::new()
            } else {
                format!(" Last log lines:\n{}", tail.join("\n"))
            };
            return Err(format!(
                "the daemon was started but never became reachable.{hint}"
            ));
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
}

/// Tunnel mode takes a passphrase and the transport picked on the Confirm screen.
pub(super) fn spawn_daemon(
    mode: ServeMode,
    passphrase: Option<&str>,
    transport: Option<TunnelTransport>,
) -> Result<(), String> {
    // Another daemon may have started while the picker was open; spawning would orphan it.
    if crate::cli::serve::daemon_pid().is_some() {
        return Err(
            "A daemon is already running. Close this dialog and reopen to see it.".to_string(),
        );
    }

    let exe = current_exe()?;

    // Stale files from a hard-killed daemon would show its URL or passphrase while starting.
    for name in ["serve.url", "serve.mode", EPHEMERAL_PASSPHRASE_FILE] {
        remove_app_file(name);
    }

    let port = load_or_generate_port();
    let mut cmd = Command::new(&exe);
    cmd.args(["serve", "--daemon", "--port", &port.to_string()]);
    match mode {
        ServeMode::Tunnel => {
            cmd.args(["--remote", "--host", "127.0.0.1"]);
            // Picking Cloudflare must skip the server's Tailscale auto-detect.
            if transport == Some(TunnelTransport::Cloudflare) {
                cmd.arg("--no-tailscale");
            }
            if let Some(pp) = passphrase {
                cmd.env("AOE_SERVE_PASSPHRASE", pp);
            }
        }
        ServeMode::Local => {
            cmd.args(["--host", "0.0.0.0"]);
        }
    }
    // The wrapper double-forks; the daemon's output lands in the log file.
    let status = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("Failed to launch `aoe serve --daemon`: {}", e))?;

    if !status.success() {
        // Forget an occupied port so the next attempt picks a fresh one.
        let tail = initial_log_tail().join("\n");
        if tail.contains("EADDRINUSE") || tail.contains("Address already in use") {
            remove_app_file("serve.last_port");
        }
        let hint = match mode {
            ServeMode::Tunnel => format!(
                "Most likely no tunnel tool is installed (install tailscale \
                 or cloudflared) or port {} is in use.",
                port
            ),
            ServeMode::Local => format!("Most likely port {} is in use.", port),
        };
        return Err(format!(
            "`aoe serve --daemon` exited with {:?}. {}",
            status.code(),
            hint
        ));
    }
    if let Some(pp) = passphrase {
        remember_passphrase(pp);
        save_passphrase_to_disk(pp);
    }
    Ok(())
}

fn current_exe() -> Result<PathBuf, String> {
    std::env::current_exe().map_err(|e| format!("Could not resolve aoe binary path: {}", e))
}

/// A blank-line-prefixed hint for a recognized errno in the daemon log, or `""`.
pub(super) fn diagnose_daemon_exit(log: &str, mode: ServeMode) -> &'static str {
    if log.contains("EADDRNOTAVAIL") || log.contains("Cannot assign requested address") {
        return match mode {
            ServeMode::Local => {
                "\n\nHint: the interface we tried to bind on went away. \
                 Is Tailscale still up?"
            }
            ServeMode::Tunnel => "",
        };
    }
    if log.contains("EADDRINUSE") || log.contains("Address already in use") {
        return "\n\nHint: the daemon couldn't bind the picked port. \
                Reopen the dialog to try again with a fresh random port.";
    }
    if log.contains("Permission denied") {
        return "\n\nHint: permission denied on bind. Are you trying a \
                privileged port (<1024)? We normally pick a high port.";
    }
    ""
}

/// Stops the daemon; the saved passphrase survives for the next launch.
pub(super) fn stop_daemon() -> Result<(), String> {
    let output = Command::new(current_exe()?)
        .args(["serve", "--stop"])
        .output()
        .map_err(|e| format!("Failed to invoke `aoe serve --stop`: {}", e))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    set_cached_passphrase(None);
    remove_app_file(EPHEMERAL_PASSPHRASE_FILE);
    Ok(())
}

pub(super) fn restart_daemon(
    mode: ServeMode,
    passphrase: Option<&str>,
    transport: Option<TunnelTransport>,
) -> Result<(), String> {
    stop_daemon()?;
    spawn_daemon(mode, passphrase, transport)
}

pub(super) fn read_serve_mode() -> Option<ServeMode> {
    ServeMode::from_file_token(&read_app_file("serve.mode")?)
}

/// Kept apart from `serve.mode` so it survives `aoe serve --stop`.
pub(super) fn read_last_mode() -> Option<ServeMode> {
    ServeMode::from_file_token(&read_app_file("serve.last_mode")?)
}

pub(super) fn remember_last_mode(mode: ServeMode) {
    if let Some(path) = app_file("serve.last_mode") {
        let _ = std::fs::write(path, mode.file_token());
    }
}

/// Reusing the last port keeps a bookmarked URL valid; a random high port avoids a user's own 8080.
fn load_or_generate_port() -> u16 {
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

fn log_file_size_opt() -> Option<u64> {
    let path = crate::cli::serve::stdio_redirect_path().ok()?;
    std::fs::metadata(path).ok().map(|m| m.len())
}

pub(super) fn log_file_size() -> u64 {
    log_file_size_opt().unwrap_or(0)
}

/// Whether the log file changed size since `offset`, updating it.
pub(super) fn log_grew(offset: &mut u64) -> bool {
    let Some(size) = log_file_size_opt() else {
        return false;
    };
    let changed = size != *offset;
    *offset = size;
    changed
}

/// The current run's lines: the log is shared, so start at the last `[AOE_START_MARKER]`.
pub(super) fn initial_log_tail() -> Vec<String> {
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

/// `"\n\nLast log lines:\n..."` with compacted lines, or `""` for an empty tail.
pub(super) fn last_log_lines(tail: &[String]) -> String {
    if tail.is_empty() {
        return String::new();
    }
    let compact: Vec<String> = tail.iter().map(|l| compact_log_line(l)).collect();
    format!("\n\nLast log lines:\n{}", compact.join("\n"))
}

/// `2026-04-19T23:43:44Z  INFO agent_of_empires::server::tunnel: msg` becomes `INFO tunnel: msg`.
fn compact_log_line(raw: &str) -> String {
    let trimmed = raw.trim_end_matches('\n');
    // Require a `YYYY-` prefix so lines like "200 OK" pass through untouched.
    let bytes = trimmed.as_bytes();
    let is_tracing =
        bytes.len() > 4 && bytes[..4].iter().all(u8::is_ascii_digit) && bytes[4] == b'-';
    if !is_tracing {
        return trimmed.to_string();
    }
    let Some((_, rest)) = trimmed.split_once("Z ") else {
        return trimmed.to_string();
    };
    let Some((level, after_level)) = rest.trim_start().split_once(' ') else {
        return trimmed.to_string();
    };
    let after_level = after_level.trim_start();
    match after_level.split_once(": ") {
        Some((path, message)) => {
            let short_path = path.rsplit("::").next().unwrap_or(path);
            format!("{level} {short_path}: {message}")
        }
        None => format!("{level} {after_level}"),
    }
}

pub(super) fn error_mentions_tailscale(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    lower.contains("tailscale") || lower.contains("funnel")
}

pub(super) fn run_tailscale_funnel_reset() -> Result<(), String> {
    let output = Command::new("tailscale")
        .args(["funnel", "reset"])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("could not spawn tailscale: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(if stderr.is_empty() {
        format!("exited with status {:?}", output.status.code())
    } else {
        stderr
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passphrase_is_four_words_from_a_lowercase_list() {
        assert!(PASSPHRASE_WORDS.len() >= 256);
        assert!(PASSPHRASE_WORDS
            .iter()
            .all(|w| !w.is_empty() && w.chars().all(|c| c.is_ascii_lowercase())));
        let pw = generate_passphrase();
        let words: Vec<&str> = pw.split(' ').collect();
        assert_eq!(words.len(), 4, "{pw:?}");
        assert!(words.iter().all(|w| PASSPHRASE_WORDS.contains(w)), "{pw:?}");
    }

    #[test]
    fn compact_log_line_shortens_only_tracing_lines() {
        let cases = [
            (
                "2026-04-19T23:43:44.609396Z  INFO agent_of_empires::server::tunnel: Warning: funnel=on",
                "INFO tunnel: Warning: funnel=on",
            ),
            (
                "2026-04-19T23:43:44.669741Z ERROR agent_of_empires::server: boom",
                "ERROR server: boom",
            ),
            (
                "Available on the internet: https://foo.ts.net",
                "Available on the internet: https://foo.ts.net",
            ),
            // Regression: digit-prefixed non-tracing lines were once mangled.
            ("200 OK received", "200 OK received"),
        ];
        for (input, expected) in cases {
            assert_eq!(compact_log_line(input), expected);
        }
    }

    #[test]
    fn diagnose_daemon_exit_recognizes_common_errnos() {
        let unavailable = "ERROR: bind: Cannot assign requested address";
        assert!(diagnose_daemon_exit(unavailable, ServeMode::Local).contains("interface"));
        assert_eq!(diagnose_daemon_exit(unavailable, ServeMode::Tunnel), "");
        assert!(diagnose_daemon_exit("Address already in use", ServeMode::Local).contains("port"));
        assert!(diagnose_daemon_exit("Permission denied", ServeMode::Tunnel).contains("permission"));
        assert_eq!(diagnose_daemon_exit("unrelated", ServeMode::Local), "");
    }

    /// One test: the cache is process-global.
    #[test]
    fn passphrase_cache_roundtrip() {
        set_cached_passphrase(None);
        assert_eq!(cached_passphrase(), None);
        remember_passphrase("four word diceware phrase");
        remember_passphrase("a different phrase later");
        assert_eq!(
            cached_passphrase().as_deref(),
            Some("a different phrase later")
        );
        set_cached_passphrase(None);
        assert_eq!(cached_passphrase(), None);
    }

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
