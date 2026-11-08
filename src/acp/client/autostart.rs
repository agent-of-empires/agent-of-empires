//! Port selection and startup log diagnostics.

use rand::RngExt;
use std::path::Path;

const LOG_TAIL_LINES: usize = 200;

pub(crate) fn load_or_generate_port() -> u16 {
    match crate::session::get_app_dir() {
        Ok(dir) => load_or_generate_port_in(&dir),
        Err(_) => random_port(),
    }
}

const MIN_PORT: u16 = 49152;

/// The port the next start reuses, if any; `None` means it picks a random one.
pub(crate) fn saved_port() -> Option<u16> {
    saved_port_in(&crate::session::get_app_dir().ok()?)
}

fn saved_port_in(dir: &Path) -> Option<u16> {
    std::fs::read_to_string(dir.join("serve.last_port"))
        .ok()
        .and_then(|raw| raw.trim().parse::<u16>().ok())
        .filter(|port| *port >= MIN_PORT)
}

fn load_or_generate_port_in(dir: &Path) -> u16 {
    saved_port_in(dir).unwrap_or_else(|| {
        let port = random_port();
        let _ = std::fs::write(dir.join("serve.last_port"), port.to_string());
        port
    })
}

fn random_port() -> u16 {
    rand::rng().random_range(MIN_PORT..65535)
}

/// The current run's lines: the log is shared, so start at the last `[AOE_START_MARKER]`.
pub(crate) fn initial_log_tail() -> Vec<String> {
    log_tail(None)
}

pub(crate) fn startup_log_tail(pid: u32) -> Vec<String> {
    log_tail(Some(pid))
}

fn log_tail(pid: Option<u32>) -> Vec<String> {
    let Some(contents) = crate::cli::serve::stdio_redirect_path()
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
    else {
        return Vec::new();
    };
    let all: Vec<&str> = contents.lines().collect();
    let marker = pid.map(|pid| format!("pid={pid}"));
    let from = all.iter().rposition(|line| {
        line.contains("[AOE_START_MARKER]")
            && marker
                .as_ref()
                .is_none_or(|marker| line.split_whitespace().any(|word| word == marker))
    });
    let from = match (from, pid) {
        (Some(from), _) => from,
        (None, None) => 0,
        (None, Some(_)) => return Vec::new(),
    };
    let end = if pid.is_some() {
        all[from + 1..]
            .iter()
            .position(|line| line.contains("[AOE_START_MARKER]"))
            .map_or(all.len(), |next| from + 1 + next)
    } else {
        all.len()
    };
    let window = &all[from..end];
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
            assert_eq!(saved_port_in(tmp.path()), reused, "{saved:?}");
            // Read-only: a probe must not create the file the next spawn writes.
            assert_eq!(path.exists(), saved.is_some(), "{saved:?}");
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
