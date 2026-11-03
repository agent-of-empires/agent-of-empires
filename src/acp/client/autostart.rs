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
