//! The boot spinner: one status row redrawn in place while startup migrations
//! run, plus the notices they emit, kept above it.

use std::io::{self, Write};

use crate::migrations::progress::{fit_width, ConsoleProgress};

const FRAMES: &[char] = &['◐', '◓', '◑', '◒'];

/// Owns at most one rendered row. Nothing reaches the terminal until a
/// migration has something to show, so a schema-current start writes no
/// bytes at all, and the row is erased only when this spinner drew it.
#[derive(Default)]
pub(super) struct BootSpinner {
    drawn: bool,
}

impl BootSpinner {
    /// Print the notices queued since the last call, then redraw the status
    /// row for `frame` if a migration is running.
    pub(super) fn draw(
        &mut self,
        out: &mut impl Write,
        console: &mut ConsoleProgress,
        frame: usize,
        width: usize,
    ) -> io::Result<()> {
        let lines = console.take_lines();
        let status = console.status_line();
        if !self.drawn && lines.is_empty() && status.is_none() {
            return Ok(());
        }
        self.erase(out)?;
        for line in lines {
            writeln!(out, "  {line}")?;
        }
        if let Some(status) = status {
            // One row only: the erase cannot reach a wrapped line.
            let line = format!("  {} {status}", FRAMES[frame % FRAMES.len()]);
            write!(out, "{}", fit_width(&line, width))?;
            self.drawn = true;
        }
        out.flush()
    }

    /// Erase the status row, if any, and print the remaining notices.
    pub(super) fn finish(
        &mut self,
        out: &mut impl Write,
        console: &mut ConsoleProgress,
    ) -> io::Result<()> {
        let lines = console.take_lines();
        if !self.drawn && lines.is_empty() {
            return Ok(());
        }
        self.erase(out)?;
        for line in lines {
            writeln!(out, "  {line}")?;
        }
        out.flush()
    }

    fn erase(&mut self, out: &mut impl Write) -> io::Result<()> {
        if self.drawn {
            write!(out, "\r\x1b[2K")?;
            self.drawn = false;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrations::progress::Event;
    use std::time::Duration;

    const ERASE: &str = "\r\x1b[2K";

    fn started() -> Event {
        Event::Started {
            version: 27,
            name: "isolate_sandbox_stores",
            position: 1,
            total: 1,
        }
    }

    fn draw(spinner: &mut BootSpinner, console: &mut ConsoleProgress, frame: usize) -> String {
        let mut out = Vec::new();
        spinner.draw(&mut out, console, frame, 80).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn spinner_touches_the_terminal_only_when_it_has_something_to_show() {
        let mut spinner = BootSpinner::default();
        let mut console = ConsoleProgress::default();

        // A schema-current start: the first tick fires at once and draws nothing.
        assert_eq!(draw(&mut spinner, &mut console, 0), "");
        let mut out = Vec::new();
        spinner.finish(&mut out, &mut console).unwrap();
        assert!(out.is_empty(), "final cleanup with nothing drawn: {out:?}");

        // A notice with no status prints as a line, with no erase first.
        console.apply(Event::Notice("kept".into()));
        assert_eq!(draw(&mut spinner, &mut console, 0), "  kept\n");

        // The first status row is drawn without erasing anything.
        console.apply(started());
        console.apply(Event::Step("reading".into()));
        let first = draw(&mut spinner, &mut console, 0);
        assert!(first.starts_with("  ◐ Data migration v27"), "{first:?}");
        assert!(!first.contains(ERASE));

        // Replacing it erases the row it owns; a notice goes above the new row.
        console.apply(Event::Notice("deferred".into()));
        let second = draw(&mut spinner, &mut console, 1);
        assert!(
            second.starts_with(&format!("{ERASE}  deferred\n  ◓ ")),
            "{second:?}"
        );
        assert_eq!(second.matches(ERASE).count(), 1);

        // Final cleanup erases the row once and keeps the summary line.
        console.apply(Event::Finished {
            version: 27,
            elapsed: Duration::from_millis(1500),
        });
        let mut out = Vec::new();
        spinner.finish(&mut out, &mut console).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(out.starts_with(ERASE), "{out:?}");
        assert!(out.ends_with("done in 1.5s\n"), "{out:?}");
        // Nothing is owned any more, so a further cleanup is silent.
        let mut out = Vec::new();
        spinner.finish(&mut out, &mut console).unwrap();
        assert!(out.is_empty());
    }
}
