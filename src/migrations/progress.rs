//! Progress events for startup data migrations.
//!
//! A migration can run for minutes (copying agent stores, probing a container
//! runtime). With nothing but a spinner the user cannot tell work from a hang.
//! The runner installs a reporter for the duration of a run and migrations
//! call `step`, `progress` and `notice` from wherever the work happens; with
//! no reporter installed every call is a no-op.

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// A migration begins. `position` and `total` count this run's pending set.
    Started {
        version: u32,
        name: &'static str,
        position: usize,
        total: usize,
    },
    /// A discrete phase of the current migration, replacing the previous one.
    Step(String),
    /// A frequent refinement of the current step (bytes copied so far). Line
    /// renderers without in-place updates drop these.
    Progress(String),
    /// A line worth keeping on screen: a deferral, something skipped, a hint.
    Notice(String),
    Finished {
        version: u32,
        elapsed: Duration,
    },
}

pub type Reporter = Arc<dyn Fn(Event) + Send + Sync>;

static REPORTER: RwLock<Option<Reporter>> = RwLock::new(None);

/// Installs `reporter` until the returned guard drops.
pub(super) fn install(reporter: Option<Reporter>) -> ReporterGuard {
    *REPORTER.write().unwrap_or_else(|e| e.into_inner()) = reporter;
    ReporterGuard
}

pub(super) struct ReporterGuard;

impl Drop for ReporterGuard {
    fn drop(&mut self) {
        *REPORTER.write().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

pub(crate) fn report(event: Event) {
    let reporter = REPORTER.read().unwrap_or_else(|e| e.into_inner()).clone();
    if let Some(reporter) = reporter {
        reporter(event);
    }
}

pub(crate) fn step(message: impl Into<String>) {
    report(Event::Step(message.into()));
}

pub(crate) fn progress(message: impl Into<String>) {
    report(Event::Progress(message.into()));
}

pub(crate) fn notice(message: impl Into<String>) {
    report(Event::Notice(message.into()));
}

/// Renders events into one status line plus a queue of lines to keep, for a
/// terminal that redraws the status line in place (the TUI boot spinner, a CLI
/// on a TTY).
#[derive(Default)]
pub struct ConsoleProgress {
    label: String,
    step: String,
    detail: String,
    started: Option<Instant>,
    lines: Vec<String>,
}

impl ConsoleProgress {
    pub fn apply(&mut self, event: Event) {
        match event {
            Event::Started {
                version,
                name,
                position,
                total,
            } => {
                self.label = if total > 1 {
                    format!("Data migration {position}/{total} (v{version} {name})")
                } else {
                    format!("Data migration v{version} ({name})")
                };
                self.step.clear();
                self.detail.clear();
                self.started = Some(Instant::now());
            }
            Event::Step(step) => {
                self.step = step;
                self.detail.clear();
            }
            Event::Progress(detail) => self.detail = detail,
            Event::Notice(line) => self.lines.push(line),
            Event::Finished { elapsed, .. } => {
                // Sub-second migrations are not worth a line.
                if elapsed >= Duration::from_secs(1) {
                    self.lines.push(format!(
                        "{} done in {}",
                        self.label,
                        format_elapsed(elapsed)
                    ));
                }
                self.started = None;
            }
        }
    }

    /// The current activity, or `None` when no migration is running.
    pub fn status_line(&self) -> Option<String> {
        let started = self.started?;
        let mut line = self.label.clone();
        if !self.step.is_empty() {
            line.push_str(": ");
            line.push_str(&self.step);
        }
        if !self.detail.is_empty() {
            line.push_str(", ");
            line.push_str(&self.detail);
        }
        line.push_str(&format!(" ({})", format_elapsed(started.elapsed())));
        Some(line)
    }

    /// Lines queued since the last call, oldest first.
    pub fn take_lines(&mut self) -> Vec<String> {
        std::mem::take(&mut self.lines)
    }
}

pub fn format_elapsed(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    if secs >= 60 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else if secs >= 10 {
        format!("{secs}s")
    } else {
        format!("{:.1}s", elapsed.as_secs_f64())
    }
}

pub fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    if bytes >= 10 * MB {
        format!("{} MB", bytes / MB)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{} KB", bytes / KB)
    } else {
        format!("{bytes} B")
    }
}

/// Clamp `line` to `width` columns by eliding its middle, so a status line
/// redrawn in place with `\r` + erase-line never wraps onto rows the erase
/// cannot reach. Paths make these lines long; keeping both ends keeps the
/// counts and the elapsed time readable.
pub fn fit_width(line: &str, width: usize) -> String {
    let chars: Vec<char> = line.chars().collect();
    if width == 0 || chars.len() <= width {
        return line.to_string();
    }
    if width < 8 {
        return chars[..width].iter().collect();
    }
    let keep = width - 1;
    let head = keep / 2;
    let tail = keep - head;
    let mut out: String = chars[..head].iter().collect();
    out.push('\u{2026}');
    out.extend(chars[chars.len() - tail..].iter());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // The reporter is process-global; a concurrent migration test would
    // interleave its events into this sequence.
    #[test]
    #[serial_test::serial]
    fn reporter_receives_events_only_while_installed() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        {
            let sink = seen.clone();
            let _guard = install(Some(Arc::new(move |event| {
                sink.lock().unwrap().push(event)
            })));
            step("planning");
            progress("3 files");
            notice("deferred");
        }
        step("after uninstall");
        assert_eq!(
            *seen.lock().unwrap(),
            vec![
                Event::Step("planning".into()),
                Event::Progress("3 files".into()),
                Event::Notice("deferred".into()),
            ]
        );
    }

    #[test]
    fn console_progress_builds_a_status_line_and_keeps_notices() {
        let mut console = ConsoleProgress::default();
        assert_eq!(console.status_line(), None);
        console.apply(Event::Started {
            version: 27,
            name: "isolate_sandbox_stores",
            position: 1,
            total: 1,
        });
        console.apply(Event::Step("copying store 1/2".into()));
        console.apply(Event::Progress("120 files, 4.0 MB".into()));
        let line = console.status_line().unwrap();
        assert!(line.starts_with(
            "Data migration v27 (isolate_sandbox_stores): copying store 1/2, 120 files, 4.0 MB ("
        ));
        // A new step drops the stale detail.
        console.apply(Event::Step("retiring legacy store".into()));
        assert!(!console.status_line().unwrap().contains("120 files"));
        console.apply(Event::Notice(
            "session abc is running; its store moves after it stops".into(),
        ));
        console.apply(Event::Finished {
            version: 27,
            elapsed: Duration::from_millis(2500),
        });
        assert_eq!(console.status_line(), None);
        let lines = console.take_lines();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("session abc"));
        assert!(lines[1].ends_with("done in 2.5s"));
        assert!(console.take_lines().is_empty());
        // Instant finishes stay quiet; a multi-migration run numbers its label.
        console.apply(Event::Started {
            version: 26,
            name: "x",
            position: 2,
            total: 3,
        });
        assert!(console
            .status_line()
            .unwrap()
            .starts_with("Data migration 2/3 (v26 x)"));
        console.apply(Event::Finished {
            version: 26,
            elapsed: Duration::from_millis(20),
        });
        assert!(console.take_lines().is_empty());
    }

    #[test]
    fn formatting_helpers_pick_readable_units() {
        assert_eq!(format_elapsed(Duration::from_millis(1500)), "1.5s");
        assert_eq!(format_elapsed(Duration::from_secs(42)), "42s");
        assert_eq!(format_elapsed(Duration::from_secs(125)), "2m05s");
        assert_eq!(format_bytes(500), "500 B");
        assert_eq!(format_bytes(512 * 1024), "512 KB");
        assert_eq!(format_bytes(3 * 1024 * 1024 / 2), "1.5 MB");
        assert_eq!(format_bytes(40 * 1024 * 1024), "40 MB");
    }

    #[test]
    fn fit_width_elides_the_middle_and_keeps_both_ends() {
        assert_eq!(fit_width("short", 80), "short");
        assert_eq!(fit_width("short", 0), "short");
        let long =
            "copying store 1/2: /home/u/.gemini/sandbox -> /home/u/.gemini/sandbox-v2/abc (0.4s)";
        let fitted = fit_width(long, 40);
        assert_eq!(fitted.chars().count(), 40);
        assert!(fitted.starts_with("copying store 1/2: "));
        assert!(fitted.ends_with(" (0.4s)"));
        assert!(fitted.contains('\u{2026}'));
        assert_eq!(fit_width("abcdefghij", 5), "abcde");
    }
}
