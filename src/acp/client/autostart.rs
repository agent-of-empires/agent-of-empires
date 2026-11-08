//! Port selection and startup log diagnostics.

use rand::RngExt;
use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;
use tokio_util::sync::CancellationToken;

const LOG_TAIL_LINES: usize = 200;
const LOG_LINE_BYTES: usize = 8 * 1024;

#[derive(Clone, Debug, Default)]
pub(crate) struct LogContext {
    pub(crate) lines: Vec<String>,
    pub(crate) has_process_marker: bool,
}

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

/// Recent shared context, without a claimed process association.
pub(crate) fn initial_log_tail() -> LogContext {
    log_tail(None, None)
}

/// A matching physical marker bounds context, but never attributes individual lines.
pub(crate) fn startup_log_tail(
    pid: u32,
    birth: &crate::process::ProcessBirthIdentity,
    cancel: &CancellationToken,
) -> LogContext {
    log_tail(Some((pid, birth)), Some(cancel))
}

fn log_tail(
    expected: Option<(u32, &crate::process::ProcessBirthIdentity)>,
    cancel: Option<&CancellationToken>,
) -> LogContext {
    if cancel.is_some_and(CancellationToken::is_cancelled) {
        return LogContext::default();
    }
    let Ok((path, config)) = crate::cli::serve::stdio_redirect_logging() else {
        return LogContext::default();
    };
    let policy = crate::logging::RotationPolicy::from(&config);
    let keep = if expected.is_some()
        && !matches!(policy.kind, crate::session::config::RotationKind::Never)
    {
        policy.keep_count
    } else {
        0
    };
    let mut newer = VecDeque::new();
    for index in 0..=keep {
        if cancel.is_some_and(CancellationToken::is_cancelled) {
            return LogContext::default();
        }
        let rotated =
            (index != 0).then(|| crate::logging::path_with_suffix(&path, &format!(".{index}")));
        let candidate = rotated.as_deref().unwrap_or(&path);
        let Ok(Some((file, length))) = crate::session::ResolvedDataFile::open(candidate)
            .and_then(|file| file.open_read_snapshot())
        else {
            continue;
        };
        let remaining = LOG_TAIL_LINES - newer.len();
        let (mut older, found) = match scan_log_file(file, length, expected, remaining, cancel) {
            Ok(context) => context,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
                return LogContext::default();
            }
            Err(_) => continue,
        };
        if !older.is_empty() {
            older.append(&mut newer);
            newer = older;
        }
        if found || expected.is_none() {
            return LogContext {
                lines: newer.into(),
                has_process_marker: found && expected.is_some(),
            };
        }
    }
    LogContext {
        lines: newer.into(),
        has_process_marker: false,
    }
}

fn scan_log_file(
    mut file: File,
    length: u64,
    expected: Option<(u32, &crate::process::ProcessBirthIdentity)>,
    limit: usize,
    cancel: Option<&CancellationToken>,
) -> std::io::Result<(VecDeque<String>, bool)> {
    let start = if expected.is_none() {
        length.saturating_sub((LOG_TAIL_LINES * LOG_LINE_BYTES) as u64)
    } else {
        0
    };
    if start != 0 {
        file.seek(SeekFrom::Start(start))?;
    }
    let mut reader = BufReader::new(file.take(length - start));
    let mut bytes = Vec::with_capacity(LOG_LINE_BYTES);
    let mut tail = VecDeque::with_capacity(if limit == 0 { 0 } else { LOG_TAIL_LINES });
    let mut continuation = start != 0;
    let mut found = false;
    loop {
        if cancel.is_some_and(CancellationToken::is_cancelled) {
            return Err(std::io::ErrorKind::Interrupted.into());
        }
        bytes.clear();
        if reader
            .by_ref()
            .take(LOG_LINE_BYTES as u64)
            .read_until(b'\n', &mut bytes)?
            == 0
        {
            break;
        }
        let complete = bytes.last() == Some(&b'\n');
        if !continuation {
            if let Ok(line) = std::str::from_utf8(&bytes) {
                let line = line.trim_end_matches('\n');
                if matches_start_marker(line, expected) {
                    found = true;
                    tail.clear();
                }
                if limit != 0 {
                    let mut stored = if tail.len() == limit {
                        tail.pop_front().unwrap()
                    } else {
                        String::new()
                    };
                    stored.clear();
                    stored.push_str(line);
                    tail.push_back(stored);
                }
            }
        }
        continuation = !complete;
    }
    Ok((tail, found))
}

fn matches_start_marker(
    line: &str,
    expected: Option<(u32, &crate::process::ProcessBirthIdentity)>,
) -> bool {
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

    #[test]
    #[serial_test::serial]
    fn startup_context_survives_missing_and_rotated_markers() {
        let _home = crate::session::test_support::isolate_app_dir();
        let app = crate::session::get_app_dir().unwrap();
        std::fs::write(
            app.join("config.toml"),
            "[logging]\nfile_path = \"context.log\"\nrotation = \"size\"\nkeep_count = 2\n",
        )
        .unwrap();
        let path = crate::cli::serve::stdio_redirect_path().unwrap();
        let retained = [
            crate::logging::path_with_suffix(&path, ".1"),
            crate::logging::path_with_suffix(&path, ".2"),
        ];
        let birth = crate::process::ProcessBirthIdentity {
            boot_id: "fixture-boot".to_owned(),
            start_id: "fixture-start".to_owned(),
        };
        for (archive, physical) in [
            (Some(1), true),
            (Some(2), true),
            (None, false),
            (Some(1), false),
        ] {
            let marker = if physical {
                "[AOE_START_MARKER] pid=17 boot_id=fixture-boot start_id=fixture-start\n"
            } else if archive.is_some() {
                "[AOE_START_MARKER] pid=17 boot_id=fixture-boot start_id=historical\n"
            } else {
                "[AOE_START_MARKER] pid=17 exe=legacy\n"
            };
            let earlier = if archive.is_some() {
                "older fixture context"
            } else {
                "late fixture context"
            };
            std::fs::write(&path, format!("prior-run-context\n{marker}{earlier}\n")).unwrap();
            if let Some(index) = archive {
                std::fs::rename(&path, &retained[index - 1]).unwrap();
                std::fs::write(
                    &path,
                    "[AOE_START_MARKER] pid=23 exe=another-client\nlate fixture context\n",
                )
                .unwrap();
            }
            let context = startup_log_tail(17, &birth, &CancellationToken::new());
            assert!(
                context
                    .lines
                    .iter()
                    .any(|line| line == "late fixture context"),
                "lost shared context; archive={archive:?}; physical={physical}"
            );
            assert_eq!(context.has_process_marker, physical);
            if physical {
                assert!(!context.lines.iter().any(|line| line == "prior-run-context"));
            }
            if archive.is_some() {
                let earlier = context
                    .lines
                    .iter()
                    .position(|line| line == "older fixture context")
                    .unwrap();
                let later = context
                    .lines
                    .iter()
                    .position(|line| line == "late fixture context")
                    .unwrap();
                assert!(earlier < later);
            }
            for file in &retained {
                let _ = std::fs::remove_file(file);
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn context_retains_bounded_lines_and_refuses_cancelled_reads() {
        let _home = crate::session::test_support::isolate_app_dir();
        let app = crate::session::get_app_dir().unwrap();
        std::fs::write(
            app.join("config.toml"),
            "[logging]\nfile_path = \"bounded.log\"\nrotation = \"size\"\nkeep_count = 2\n",
        )
        .unwrap();
        let birth = crate::process::ProcessBirthIdentity {
            boot_id: "fixture-boot".to_owned(),
            start_id: "fixture-start".to_owned(),
        };
        let mut contents = String::from(
            "[AOE_START_MARKER] pid=17 boot_id=fixture-boot start_id=fixture-start\n\
             must-not-survive\n",
        );
        contents.push_str(&"older context\n".repeat(LOG_TAIL_LINES + 1));
        contents.push_str("large-start-");
        contents.push_str(&"x".repeat(LOG_LINE_BYTES * 2));
        contents.push_str("\nlast fixture context\n");
        std::fs::write(crate::cli::serve::stdio_redirect_path().unwrap(), contents).unwrap();
        let cancel = CancellationToken::new();
        let context = startup_log_tail(17, &birth, &cancel);
        assert!(context.has_process_marker);
        assert_eq!(context.lines.last().unwrap(), "last fixture context");
        assert!(context
            .lines
            .iter()
            .any(|line| line.starts_with("large-start-")));
        assert!(context
            .lines
            .iter()
            .all(|line| line.len() <= LOG_LINE_BYTES));
        assert!(!context.lines.iter().any(|line| line == "must-not-survive"));
        cancel.cancel();
        let cancelled = startup_log_tail(17, &birth, &cancel);
        assert!(cancelled.lines.is_empty());
        assert!(!cancelled.has_process_marker);
    }
}
