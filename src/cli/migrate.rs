//! `aoe migrate`: run pending data migrations now, with progress on stderr.
//!
//! Every `aoe` command runs pending migrations before it starts; this command
//! exists so a user who deferred a long one (see
//! [`crate::migrations::v027_isolate_sandbox_stores::DEFER_ENV`]) can finish
//! it when convenient and watch it happen.

use std::io::{IsTerminal, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::migrations::progress::{ConsoleProgress, Event, Reporter};

/// How long a migration may run before a CLI command mentions it. Quick
/// migrations stay silent; anything slower gets its name and steps.
const QUIET_FOR: Duration = Duration::from_millis(1500);

/// A reporter that writes migration progress to stderr. Notices always print.
/// The migration's name and steps print once it has run for [`QUIET_FOR`] (or
/// once a notice makes the context necessary); on a TTY the status line is
/// redrawn in place with per-file progress, on a pipe steps print as lines.
pub fn stderr_reporter() -> Reporter {
    let tty = std::io::stderr().is_terminal();
    let state = Arc::new(Mutex::new((
        ConsoleProgress::default(),
        None::<Instant>,
        false,
    )));
    Arc::new(move |event: Event| {
        let mut guard = state.lock().unwrap_or_else(|e| e.into_inner());
        let (console, started, shown) = &mut *guard;
        match &event {
            Event::Started { .. } => {
                *started = Some(Instant::now());
                *shown = false;
            }
            Event::Finished { .. } => *started = None,
            _ => {}
        }
        let discrete = !matches!(event, Event::Progress(_));
        let notice = matches!(event, Event::Notice(_));
        console.apply(event);
        let slow = started.is_some_and(|at| at.elapsed() >= QUIET_FOR);
        if notice || slow {
            *shown = true;
        }
        let lines = console.take_lines();
        if !*shown && lines.is_empty() {
            return;
        }
        let mut err = std::io::stderr().lock();
        if tty {
            let _ = write!(err, "\r\x1b[2K");
        }
        for line in lines {
            let _ = writeln!(err, "aoe: {line}");
        }
        if let Some(status) = console.status_line() {
            if tty {
                let _ = write!(err, "aoe: {status}");
            } else if discrete && !notice {
                let _ = writeln!(err, "aoe: {status}");
            }
        }
        let _ = err.flush();
    })
}

pub fn run() -> Result<()> {
    if !crate::migrations::has_pending_migrations() {
        eprintln!("aoe: data schema is current; checking for deferred sandbox store moves");
    }
    crate::migrations::run_migrations_announced(Some(stderr_reporter()))?;
    if std::io::stderr().is_terminal() {
        eprint!("\r\x1b[2K");
    }
    eprintln!("aoe: migrations complete");
    Ok(())
}
