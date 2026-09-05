//! `aoe migrate`: run pending data migrations now, with progress on stderr.
//!
//! Every `aoe` command runs pending migrations before it starts; this command
//! exists so a user who deferred a long one (see `DEFER_ENV` in
//! `crate::migrations::v027_isolate_sandbox_stores`) can finish it when
//! convenient and watch it happen.

use std::io::{IsTerminal, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::migrations::progress::{fit_width, ConsoleProgress, Event, Reporter};

/// How long a migration may run before a CLI command mentions it. Quick
/// migrations stay silent; anything slower gets its name and steps.
const QUIET_FOR: Duration = Duration::from_millis(1500);

/// Columns assumed when the terminal width is unknown.
const FALLBACK_WIDTH: usize = 80;

const PREFIX: &str = "aoe: ";

/// Renders migration events into stderr writes. Notices always print. The
/// migration's name and steps print once it has run for `QUIET_FOR` (or once
/// a notice makes the context necessary). On a TTY the status line is redrawn
/// in place and clamped to the terminal width; on a pipe each new step prints
/// as its own line and per-file progress is dropped.
struct StderrRenderer {
    console: ConsoleProgress,
    started: Option<Instant>,
    shown: bool,
    tty: bool,
}

impl StderrRenderer {
    fn new(tty: bool) -> Self {
        Self {
            console: ConsoleProgress::default(),
            started: None,
            shown: false,
            tty,
        }
    }

    /// The bytes to write for `event`, observed at `now` on a `width`-column
    /// terminal.
    fn render(&mut self, event: Event, now: Instant, width: usize) -> String {
        match &event {
            Event::Started { .. } => {
                self.started = Some(now);
                self.shown = false;
            }
            Event::Finished { .. } => self.started = None,
            _ => {}
        }
        let discrete = !matches!(event, Event::Progress(_));
        let notice = matches!(event, Event::Notice(_));
        self.console.apply(event);
        let slow = self
            .started
            .is_some_and(|at| now.duration_since(at) >= QUIET_FOR);
        let newly_shown = !self.shown && (notice || slow);
        if notice || slow {
            self.shown = true;
        }
        let lines = self.console.take_lines();
        if !self.shown && lines.is_empty() {
            return String::new();
        }
        let mut out = String::new();
        if self.tty {
            out.push_str("\r\x1b[2K");
        }
        for line in lines {
            out.push_str(PREFIX);
            out.push_str(&line);
            out.push('\n');
        }
        if let Some(status) = self.console.status_line() {
            if self.tty {
                out.push_str(&fit_width(
                    &format!("{PREFIX}{status}"),
                    width.max(FALLBACK_WIDTH.min(width)),
                ));
            } else if !notice && (discrete || newly_shown) {
                // A pipe gets each new step once; the first line after the
                // quiet period also goes out even when a progress tick is
                // what crossed it, so a slow copy is never silent. A notice
                // stands alone; the status follows with the next step.
                out.push_str(PREFIX);
                out.push_str(&status);
                out.push('\n');
            }
        }
        out
    }
}

pub fn stderr_reporter() -> Reporter {
    let tty = std::io::stderr().is_terminal();
    let renderer = Arc::new(Mutex::new(StderrRenderer::new(tty)));
    Arc::new(move |event: Event| {
        let width = crate::terminal::get_size().map_or(FALLBACK_WIDTH, |(w, _)| w as usize);
        let out =
            renderer
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .render(event, Instant::now(), width);
        if out.is_empty() {
            return;
        }
        let mut err = std::io::stderr().lock();
        let _ = err.write_all(out.as_bytes());
        let _ = err.flush();
    })
}

pub fn run() -> Result<()> {
    if !crate::migrations::has_pending_migrations() {
        eprintln!("{PREFIX}data schema is current; retrying any deferred migration work");
    }
    crate::migrations::run_migrations_announced(Some(stderr_reporter()))?;
    if std::io::stderr().is_terminal() {
        eprint!("\r\x1b[2K");
    }
    eprintln!("{PREFIX}migrations complete");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn started() -> Event {
        Event::Started {
            version: 27,
            name: "isolate_sandbox_stores",
            position: 1,
            total: 1,
        }
    }

    #[test]
    fn a_quick_migration_stays_silent_on_a_pipe() {
        let mut r = StderrRenderer::new(false);
        let t0 = Instant::now();
        assert_eq!(r.render(started(), t0, 80), "");
        assert_eq!(
            r.render(Event::Step("reading session registries".into()), t0, 80),
            ""
        );
        assert_eq!(
            r.render(
                Event::Finished {
                    version: 27,
                    elapsed: Duration::from_millis(300),
                },
                t0,
                80,
            ),
            ""
        );
    }

    #[test]
    fn a_slow_migration_prints_its_first_status_even_from_a_progress_tick() {
        let mut r = StderrRenderer::new(false);
        let t0 = Instant::now();
        r.render(started(), t0, 80);
        r.render(Event::Step("copying agent store 1/2".into()), t0, 80);
        // Still inside the quiet period: nothing.
        assert_eq!(
            r.render(
                Event::Progress("100 files, 5 MB".into()),
                t0 + Duration::from_millis(500),
                80
            ),
            ""
        );
        // The tick that crosses the quiet period prints the status once.
        let out = r.render(
            Event::Progress("200 files, 11 MB".into()),
            t0 + QUIET_FOR,
            80,
        );
        assert!(out.starts_with(
            "aoe: Data migration v27 (isolate_sandbox_stores): copying agent store 1/2, 200 files"
        ));
        assert!(out.ends_with('\n'));
        // Later ticks stay quiet on a pipe; a new step prints.
        assert_eq!(
            r.render(
                Event::Progress("300 files, 17 MB".into()),
                t0 + QUIET_FOR,
                80
            ),
            ""
        );
        let out = r.render(
            Event::Step("retiring shared agent store".into()),
            t0 + QUIET_FOR,
            80,
        );
        assert_eq!(out.matches('\n').count(), 1);
        assert!(out.contains("retiring shared agent store"));
    }

    #[test]
    fn a_notice_prints_at_once_and_reveals_the_context() {
        let mut r = StderrRenderer::new(false);
        let t0 = Instant::now();
        r.render(started(), t0, 80);
        r.render(Event::Step("reading session registries".into()), t0, 80);
        let out = r.render(
            Event::Notice("AOE_DEFER_SANDBOX_MIGRATION is set".into()),
            t0,
            80,
        );
        assert_eq!(out, "aoe: AOE_DEFER_SANDBOX_MIGRATION is set\n");
        // Context is now shown: the next step prints as a line.
        let out = r.render(
            Event::Step("checking which sandbox containers are running".into()),
            t0,
            80,
        );
        assert_eq!(out, "aoe: Data migration v27 (isolate_sandbox_stores): checking which sandbox containers are running (0.0s)\n");
    }

    #[test]
    fn a_tty_redraws_one_clamped_status_line() {
        let mut r = StderrRenderer::new(true);
        let t0 = Instant::now();
        r.render(started(), t0, 40);
        let step = "copying agent store 1/2: /home/u/.gemini/sandbox -> /home/u/.gemini/sandbox-v2/0123456789abcdef";
        let out = r.render(Event::Step(step.into()), t0 + QUIET_FOR, 40);
        assert!(out.starts_with("\r\x1b[2K"));
        let status = out.trim_start_matches("\r\x1b[2K");
        assert_eq!(status.chars().count(), 40, "{status:?}");
        assert!(!status.contains('\n'));
        // A finished migration leaves its summary line and clears the status.
        let out = r.render(
            Event::Finished {
                version: 27,
                elapsed: Duration::from_millis(2500),
            },
            t0 + QUIET_FOR,
            40,
        );
        assert_eq!(
            out,
            "\r\x1b[2Kaoe: Data migration v27 (isolate_sandbox_stores) done in 2.5s\n"
        );
    }
}
