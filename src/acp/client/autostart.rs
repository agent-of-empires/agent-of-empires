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

/// Shared log context after the last startup marker.
pub(crate) fn initial_log_tail() -> Vec<String> {
    log_tail(None)
}

/// Shared context anchored to this physical process, not exclusive ownership of its lines.
pub(crate) fn startup_log_tail(
    pid: u32,
    birth: &crate::process::ProcessBirthIdentity,
) -> Vec<String> {
    log_tail(Some((pid, birth)))
}

fn log_tail(expected: Option<(u32, &crate::process::ProcessBirthIdentity)>) -> Vec<String> {
    let Some(contents) = crate::cli::serve::stdio_redirect_path()
        .ok()
        .and_then(|path| crate::session::ResolvedDataFile::open(&path).ok())
        .and_then(|file| file.read().ok().flatten())
    else {
        return Vec::new();
    };
    let all: Vec<&str> = contents.lines().collect();
    let from = all.iter().rposition(|line| {
        if !line.contains("[AOE_START_MARKER]") {
            return false;
        }
        let Some((pid, birth)) = expected else {
            return true;
        };
        let (mut matches_pid, mut matches_boot, mut matches_start) = (false, false, false);
        for word in line.split_whitespace() {
            match word.split_once('=') {
                Some(("pid", value)) => matches_pid = value.parse::<u32>().ok() == Some(pid),
                Some(("boot_id", value)) => matches_boot = value == birth.boot_id,
                Some(("start_id", value)) => matches_start = value == birth.start_id,
                _ => {}
            }
        }
        matches_pid && matches_boot && matches_start
    });
    let from = match (from, expected) {
        (Some(from), _) => from,
        (None, None) => 0,
        (None, Some(_)) => return Vec::new(),
    };
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
