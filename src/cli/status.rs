//! `agent-of-empires status` command implementation

use clap::Args;
use serde::Serialize;

use crate::session::Status;

#[derive(Args)]
pub struct StatusArgs {
    /// Show detailed session list
    #[arg(short = 'v', long)]
    pub(crate) verbose: bool,

    /// Only output waiting count (for scripts)
    #[arg(short = 'q', long)]
    pub(crate) quiet: bool,

    /// Output as JSON
    #[arg(long)]
    pub(crate) json: bool,
}

#[derive(Default)]
pub(crate) struct StatusCounts {
    pub(crate) running: usize,
    pub(crate) waiting: usize,
    pub(crate) idle: usize,
    pub(crate) stopped: usize,
    pub(crate) error: usize,
    pub(crate) total: usize,
}

/// Shared display order and glyphs for local and served status output.
pub(crate) const VERBOSE_GROUPS: [(&str, &str, Status); 5] = [
    ("WAITING", "⠃", Status::Waiting),
    ("RUNNING", "⠋", Status::Running),
    ("IDLE", "⠒", Status::Idle),
    ("STOPPED", "⠒", Status::Stopped),
    ("ERROR", "✕", Status::Error),
];

/// Format a nonempty verbose group from title, tool and path rows.
pub(crate) fn verbose_group(
    label: &str,
    symbol: &str,
    rows: &[(impl AsRef<str>, impl AsRef<str>, String)],
) -> String {
    use std::fmt::Write;
    if rows.is_empty() {
        return String::new();
    }
    let mut output = format!("{label} ({}):\n", rows.len());
    for (title, tool, path) in rows {
        let _ = writeln!(
            output,
            "  {symbol} {:<16} {:<10} {path}",
            title.as_ref(),
            tool.as_ref()
        );
    }
    output.push('\n');
    output
}

#[derive(Serialize)]
pub(crate) struct StatusJson {
    waiting: usize,
    running: usize,
    idle: usize,
    stopped: usize,
    error: usize,
    total: usize,
}

impl From<&StatusCounts> for StatusJson {
    fn from(counts: &StatusCounts) -> Self {
        Self {
            waiting: counts.waiting,
            running: counts.running,
            idle: counts.idle,
            stopped: counts.stopped,
            error: counts.error,
            total: counts.total,
        }
    }
}

pub(crate) fn status_json(counts: &StatusCounts) -> String {
    if counts.total == 0 {
        return "{\"waiting\": 0, \"running\": 0, \"idle\": 0, \"stopped\": 0, \"error\": 0, \"total\": 0}"
            .to_owned();
    }
    serde_json::to_string(&StatusJson::from(counts)).expect("counts serialize")
}

/// The tallies, from the statuses the rows carry.
pub(crate) fn count_statuses(statuses: impl Iterator<Item = Status>) -> StatusCounts {
    let mut counts = StatusCounts::default();
    for status in statuses {
        match status {
            Status::Running => counts.running += 1,
            Status::Waiting => counts.waiting += 1,
            Status::Idle => counts.idle += 1,
            Status::Unknown => counts.idle += 1,
            Status::Stopped => counts.stopped += 1,
            Status::Error => counts.error += 1,
            Status::Starting => counts.idle += 1,
            Status::Deleting => {}
            Status::Creating => {}
        }
        counts.total += 1;
    }
    counts
}
