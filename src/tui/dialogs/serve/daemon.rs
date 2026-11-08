//! Daemon process control and the `$APP_DIR/serve.*` files the view reads and writes.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use rand::prelude::IndexedRandom;

use super::words::PASSPHRASE_WORDS;
use super::{ServeMode, TransportStatus, TunnelTransport};

const SAVED_PASSPHRASE_FILE: &str = "serve.saved_passphrase";
/// Written by the server at startup, so CLI-launched daemons can show their passphrase.
const EPHEMERAL_PASSPHRASE_FILE: &str = "serve.passphrase";

fn app_file(name: &str) -> Option<PathBuf> {
    crate::session::get_app_dir().ok().map(|dir| dir.join(name))
}

pub(super) fn read_app_file(name: &str) -> Option<String> {
    let file = crate::session::ResolvedDataFile::open(&app_file(name)?).ok()?;
    let raw = file.read().ok()??;
    let trimmed = raw.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

pub(super) fn current_passphrase() -> Option<String> {
    read_app_file(EPHEMERAL_PASSPHRASE_FILE)
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
    read_app_file(SAVED_PASSPHRASE_FILE).unwrap_or_else(generate_passphrase)
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

pub(super) fn assess_transports(
    cancel: &tokio_util::sync::CancellationToken,
) -> (TransportStatus, TransportStatus) {
    let tailscale = if !crate::server::tunnel::tailscale_available_sync(cancel) {
        TransportStatus::NotInstalled
    } else if !crate::server::tunnel::tailscale_funnel_cap_ready_sync(cancel) {
        TransportStatus::FunnelNotEnabled
    } else {
        TransportStatus::Ready
    };
    let cloudflare = if crate::server::tunnel::check_cloudflared(cancel).is_ok() {
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

/// Whether the next start reuses `serve.token`. `load_or_generate_token` keeps it only while it
/// is under 24 hours old, and only remote serving refreshes it while running.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum TokenWindow {
    #[default]
    Reused,
    /// Within an hour of expiry, so a help overlay left open may be stale by the restart.
    Boundary,
    /// The next start mints a new token, which the Devices' `aoe_token` cookies don't match.
    Renews,
}

pub(super) fn token_window() -> TokenWindow {
    crate::session::get_app_dir()
        .ok()
        .map_or(TokenWindow::Renews, |dir| token_window_in(&dir))
}

fn token_window_in(dir: &Path) -> TokenWindow {
    let max_age = crate::server::TOKEN_REUSE_MAX_AGE;
    let age = std::fs::metadata(dir.join("serve.token"))
        .and_then(|meta| meta.modified())
        .ok()
        .map(|modified| modified.elapsed().unwrap_or_default());
    match age {
        None => TokenWindow::Renews,
        Some(age) if age >= max_age => TokenWindow::Renews,
        Some(age) if age >= max_age - std::time::Duration::from_secs(60 * 60) => {
            TokenWindow::Boundary
        }
        Some(_) => TokenWindow::Reused,
    }
}

fn log_file_size_opt() -> Option<u64> {
    let path = crate::cli::serve::stdio_redirect_path().ok()?;
    std::fs::metadata(path).ok().map(|m| m.len())
}

pub(super) fn log_file_size() -> u64 {
    log_file_size_opt().unwrap_or(0)
}

/// Shared context with compacted lines, or `""` for an empty tail.
pub(super) fn last_log_lines(context: &crate::acp::client::autostart::LogContext) -> String {
    let tail = &context.lines;
    if tail.is_empty() {
        return String::new();
    }
    let compact: Vec<String> = tail[tail.len().saturating_sub(8)..]
        .iter()
        .map(|line| compact_log_line(line))
        .collect();
    let label = if context.has_process_marker {
        "Shared log context (other AoE processes may appear):"
    } else {
        "Unattributed shared log context (other or earlier runs may appear):"
    };
    format!("\n\n{label}\n{}", compact.join("\n"))
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

pub(super) fn run_tailscale_funnel_reset(
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<(), String> {
    use std::io::{Read, Seek};
    let mut stderr =
        tempfile::tempfile().map_err(|e| format!("could not prepare reset diagnostics: {e}"))?;
    let capture = stderr
        .try_clone()
        .map_err(|e| format!("could not capture reset diagnostics: {e}"))?;
    let status = crate::process::run_status_with_timeout_process_group(
        Command::new("tailscale")
            .args(["funnel", "reset"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(capture)),
        std::time::Duration::from_secs(3),
        Some(cancel),
    )
    .map_err(|e| format!("could not complete tailscale reset: {e}; Funnel state may have changed"))?
    .ok_or_else(|| {
        "reset timed out; Funnel state may have changed. Inspect `tailscale funnel status` before retrying."
            .to_owned()
    })?;
    if status.success() {
        return Ok(());
    }
    stderr.rewind().map_err(|e| {
        format!("could not read reset diagnostics: {e}; Funnel state may have changed")
    })?;
    let mut diagnostic = Vec::new();
    stderr
        .take(8192)
        .read_to_end(&mut diagnostic)
        .map_err(|e| {
            format!("could not read reset diagnostics: {e}; Funnel state may have changed")
        })?;
    let stderr = String::from_utf8_lossy(&diagnostic).trim().to_string();
    Err(if stderr.is_empty() {
        format!(
            "exited with status {:?}; Funnel state may have changed",
            status.code()
        )
    } else {
        format!("{stderr}; Funnel state may have changed")
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
    fn token_window_follows_the_reuse_age() {
        use TokenWindow::{Boundary, Renews, Reused};
        let minutes = |m: u64| std::time::Duration::from_secs(m * 60);
        for (age, window) in [
            (None, Renews),
            (Some(minutes(60)), Reused),
            (Some(minutes(23 * 60 - 1)), Reused),
            (Some(minutes(23 * 60)), Boundary),
            (Some(minutes(24 * 60 - 1)), Boundary),
            (Some(minutes(24 * 60)), Renews),
            (Some(minutes(25 * 60)), Renews),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            if let Some(age) = age {
                let path = tmp.path().join("serve.token");
                std::fs::write(&path, "t").unwrap();
                std::fs::File::options()
                    .write(true)
                    .open(&path)
                    .unwrap()
                    .set_modified(std::time::SystemTime::now() - age)
                    .unwrap();
            }
            assert_eq!(token_window_in(tmp.path()), window, "{age:?}");
        }
    }
}
