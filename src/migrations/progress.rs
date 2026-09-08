//! Progress events for startup data migrations.
//!
//! A migration can run for minutes (copying agent stores, probing a container
//! runtime). With nothing but a spinner the user cannot tell work from a hang.
//! The runner installs a reporter for the duration of a run and migrations
//! call `step`, `progress` and `notice` from wherever the work happens; with
//! no reporter installed every call is a no-op.

use std::cell::RefCell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
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

/// A reporter for callers with no screen of their own: the store move on the
/// container path runs while a user waits, so it must leave a trail even when
/// nothing installed a renderer. `Progress` is dropped, being the per-100-file
/// refinement, and everything else is logged once.
pub fn tracing_reporter() -> Reporter {
    Arc::new(|event| match event {
        Event::Started { version, name, .. } => {
            tracing::info!(target: "migrations", version, name, "running migration");
        }
        Event::Step(message) => tracing::info!(target: "migrations", "{message}"),
        Event::Notice(message) => tracing::warn!(target: "migrations", "{message}"),
        Event::Finished { version, elapsed } => {
            tracing::info!(
                target: "migrations",
                version,
                duration_ms = elapsed.as_millis() as u64,
                "migration completed"
            );
        }
        Event::Progress(_) => {}
    })
}

thread_local! {
    /// Reporters installed on this thread, oldest first. Thread-local because
    /// a migration reports from the thread that runs it, and two can run at
    /// once (the boot pass and a store move from a session launch); a
    /// process-wide slot would route one's events to the other's renderer.
    static REPORTERS: RefCell<Vec<(u64, Reporter)>> = const { RefCell::new(Vec::new()) };
}

static NEXT_REPORTER_ID: AtomicU64 = AtomicU64::new(1);

/// Installs `reporter` on this thread until the returned guard drops. Events
/// go to the newest installed reporter whose guard is still alive, so guards
/// may drop in any order: each removes only its own registration. `None`
/// installs nothing and leaves an outer reporter, if any, in effect.
pub(super) fn install(reporter: Option<Reporter>) -> ReporterGuard {
    let id = reporter.map(|reporter| {
        let id = NEXT_REPORTER_ID.fetch_add(1, Ordering::Relaxed);
        REPORTERS.with(|reporters| reporters.borrow_mut().push((id, reporter)));
        id
    });
    ReporterGuard(id)
}

pub(super) struct ReporterGuard(Option<u64>);

impl Drop for ReporterGuard {
    fn drop(&mut self) {
        if let Some(id) = self.0.take() {
            REPORTERS.with(|reporters| reporters.borrow_mut().retain(|(other, _)| *other != id));
        }
    }
}

pub(crate) fn report(event: Event) {
    let reporter = REPORTERS.with(|reporters| {
        reporters
            .borrow()
            .last()
            .map(|(_, reporter)| reporter.clone())
    });
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

    /// The current activity under the migration's label, or `None` when no
    /// migration is running.
    pub fn status_line(&self) -> Option<String> {
        let activity = self.activity()?;
        Some(if self.step.is_empty() {
            format!("{} {activity}", self.label)
        } else {
            format!("{}: {activity}", self.label)
        })
    }

    /// The current step, its latest progress and the elapsed time, for a
    /// renderer with a label of its own.
    pub fn activity(&self) -> Option<String> {
        let started = self.started?;
        let mut line = self.step.clone();
        if !self.detail.is_empty() {
            line.push_str(", ");
            line.push_str(&self.detail);
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(&format!("({})", format_elapsed(started.elapsed())));
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

/// Clamp `line` to `width` terminal columns by eliding its middle, so a
/// status line redrawn in place with `\r` + erase-line never wraps onto rows
/// the erase cannot reach. Paths make these lines long; keeping both ends
/// keeps the counts and the elapsed time readable. Measured in display cells
/// over grapheme clusters, so a wide glyph counts as two columns and a
/// combining mark as none, and no cluster is ever split.
pub fn fit_width(line: &str, width: usize) -> String {
    use unicode_segmentation::UnicodeSegmentation;
    use unicode_width::UnicodeWidthStr;

    if width == 0 || line.width() <= width {
        return line.to_string();
    }
    let clusters: Vec<(&str, usize)> = line
        .graphemes(true)
        .map(|cluster| (cluster, cluster.width()))
        .collect();
    // Clusters from the front whose widths fit in `budget`.
    let take_head = |budget: usize| {
        let mut used = 0;
        clusters
            .iter()
            .take_while(|(_, cells)| {
                used += cells;
                used <= budget
            })
            .count()
    };
    if width < 8 {
        return clusters[..take_head(width)]
            .iter()
            .map(|(cluster, _)| *cluster)
            .collect();
    }
    let keep = width - 1;
    let head = take_head(keep / 2);
    let mut used = 0;
    let tail = clusters
        .iter()
        .rev()
        .take_while(|(_, cells)| {
            used += cells;
            used <= keep - keep / 2
        })
        .count();
    let mut out: String = clusters[..head]
        .iter()
        .map(|(cluster, _)| *cluster)
        .collect();
    out.push('\u{2026}');
    out.extend(
        clusters[clusters.len() - tail..]
            .iter()
            .map(|(cluster, _)| *cluster),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use unicode_segmentation::UnicodeSegmentation;

    fn recording(seen: &Arc<Mutex<Vec<Event>>>) -> Reporter {
        let sink = seen.clone();
        Arc::new(move |event| sink.lock().unwrap().push(event))
    }

    #[test]
    fn reporter_receives_events_only_while_installed() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        {
            let _guard = install(Some(recording(&seen)));
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

    /// Guards may overlap and drop in either order: the newest live reporter
    /// gets the events, a dropped guard takes only its own registration with
    /// it, and nothing is resurrected once every guard is gone.
    #[test]
    fn overlapping_guards_route_to_the_newest_live_reporter() {
        let a = Arc::new(Mutex::new(Vec::new()));
        let b = Arc::new(Mutex::new(Vec::new()));
        let drained = |seen: &Arc<Mutex<Vec<Event>>>| std::mem::take(&mut *seen.lock().unwrap());

        // Non-LIFO: the older guard drops first.
        let guard_a = install(Some(recording(&a)));
        let guard_b = install(Some(recording(&b)));
        step("both");
        drop(guard_a);
        step("b only");
        drop(guard_b);
        step("nobody");
        assert!(
            drained(&a).is_empty(),
            "a was never the newest live reporter"
        );
        assert_eq!(
            drained(&b),
            vec![Event::Step("both".into()), Event::Step("b only".into())]
        );

        // Nested LIFO: the inner guard hands back to the outer one.
        let guard_a = install(Some(recording(&a)));
        {
            let _guard_b = install(Some(recording(&b)));
            step("inner");
        }
        step("outer again");
        drop(guard_a);
        step("nobody");
        assert_eq!(drained(&a), vec![Event::Step("outer again".into())]);
        assert_eq!(drained(&b), vec![Event::Step("inner".into())]);

        // `None` installs nothing, so an outer reporter keeps receiving.
        let _guard_a = install(Some(recording(&a)));
        {
            let _silent = install(None);
            step("through none");
        }
        assert_eq!(drained(&a), vec![Event::Step("through none".into())]);
    }

    /// Reporters are per thread: a migration on another thread neither sees
    /// this thread's reporter nor disturbs it.
    #[test]
    fn reporters_do_not_cross_threads() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let _guard = install(Some(recording(&seen)));
        std::thread::spawn(|| {
            step("other thread, no reporter");
            let _inner = install(None);
        })
        .join()
        .unwrap();
        step("this thread");
        assert_eq!(
            *seen.lock().unwrap(),
            vec![Event::Step("this thread".into())]
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
        assert!(console
            .status_line()
            .unwrap()
            .starts_with("Data migration v27 (isolate_sandbox_stores) ("));
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
        use unicode_width::UnicodeWidthStr;

        assert_eq!(fit_width("short", 80), "short");
        assert_eq!(fit_width("short", 0), "short");
        let long =
            "copying store 1/2: /home/u/.gemini/sandbox -> /home/u/.gemini/sandbox-v2/abc (0.4s)";
        let fitted = fit_width(long, 40);
        assert_eq!(fitted.width(), 40);
        assert!(fitted.starts_with("copying store 1/2: "));
        assert!(fitted.ends_with(" (0.4s)"));
        assert!(fitted.contains('\u{2026}'));
        assert_eq!(fit_width("abcdefghij", 5), "abcde");

        // Width is measured in terminal cells over grapheme clusters, never
        // in chars: wide glyphs cost two columns, combining marks and
        // zero-width joiners none, and no cluster is split.
        let wide = "copying store: /home/u/\u{6f22}\u{5b57}\u{6f22}\u{5b57}\u{6f22}\u{5b57}\u{6f22}\u{5b57}/sandbox (0.4s)";
        for width in [9, 12, 20, 33] {
            let fitted = fit_width(wide, width);
            assert!(fitted.width() <= width, "{width}: {fitted:?}");
            assert!(fitted.width() >= width - 2, "{width}: {fitted:?}");
            assert!(fitted.contains('\u{2026}'));
            assert!(fitted.starts_with("cop"), "{width}: {fitted:?}");
            assert!(fitted.ends_with("s)"), "{width}: {fitted:?}");
        }
        assert_eq!(fit_width("\u{6f22}\u{5b57}\u{6f22}", 5), "\u{6f22}\u{5b57}");
        assert_eq!(fit_width("\u{6f22}\u{5b57}\u{6f22}", 4), "\u{6f22}\u{5b57}");
        assert_eq!(fit_width("\u{6f22}\u{5b57}\u{6f22}", 1), "");
        let combining =
            "e\u{301}e\u{301}e\u{301}e\u{301}e\u{301}e\u{301}e\u{301}e\u{301}e\u{301}e\u{301}";
        assert_eq!(combining.width(), 10);
        assert_eq!(
            fit_width(combining, 10),
            combining,
            "10 cells fit in 10 columns"
        );
        let fitted = fit_width(combining, 9);
        assert_eq!(fitted.width(), 9);
        assert_eq!(fitted.graphemes(true).count(), 9);
        assert!(fitted
            .graphemes(true)
            .all(|g| g == "e\u{301}" || g == "\u{2026}"));
        let family = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}";
        let joined = format!("{family}{family}{family}{family}{family}");
        let fitted = fit_width(&joined, 8);
        assert!(fitted.width() <= 8);
        assert!(
            !fitted.contains("\u{200d}\u{2026}"),
            "must not split a joiner sequence: {fitted:?}"
        );
        assert_eq!(fit_width("abcdefghij", 3), "abc");
    }
}
