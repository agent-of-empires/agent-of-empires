//! `agent-of-empires status` command implementation

use anyhow::Result;
use clap::Args;
use serde::Serialize;

use crate::session::{Status, Storage};

#[derive(Args)]
pub struct StatusArgs {
    /// Show detailed session list
    #[arg(short = 'v', long, conflicts_with_all = ["quiet", "json"])]
    pub(crate) verbose: bool,

    /// Only output waiting count (for scripts)
    #[arg(short = 'q', long, conflicts_with_all = ["json", "verbose"])]
    pub(crate) quiet: bool,

    /// Output as JSON
    #[arg(long, conflicts_with_all = ["quiet", "verbose"])]
    pub(crate) json: bool,
}

impl StatusArgs {
    #[cfg(test)]
    pub(crate) fn test_json() -> Self {
        Self {
            verbose: false,
            quiet: false,
            json: true,
        }
    }
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

/// The five groups `aoe status --verbose` prints, in the order it prints them,
/// with the glyph each one uses. The renderer iterates this same table, so a
/// group cannot be dropped or respelled on one transport only.
pub(crate) const VERBOSE_GROUPS: [(&str, &str, Status); 5] = [
    ("WAITING", "⠃", Status::Waiting),
    ("RUNNING", "⠋", Status::Running),
    ("IDLE", "⠒", Status::Idle),
    ("STOPPED", "⠒", Status::Stopped),
    ("ERROR", "✕", Status::Error),
];

/// One verbose group: the heading, one padded row per session, and the blank
/// line after it. Empty when the group has no members, exactly as the local
/// command prints nothing at all for an empty group.
pub(crate) fn verbose_group(
    label: &str,
    symbol: &str,
    rows: &[(String, String, String)],
) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let mut output = format!("{label} ({}):\n", rows.len());
    for (title, tool, path) in rows {
        output.push_str(&format!("  {symbol} {title:<16} {tool:<10} {path}\n"));
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

/// One spelling for both JSON branches, on either transport.
pub(crate) fn status_json(counts: &StatusCounts) -> String {
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

#[tracing::instrument(target = "cli.session", skip_all, fields(profile = %profile))]
pub async fn run(profile: &str, args: StatusArgs) -> Result<()> {
    let storage = Storage::open_unwatched(profile)?;
    let (mut instances, _) = storage.load_with_groups()?;
    for inst in &mut instances {
        inst.source_profile = storage.profile().to_string();
    }

    if instances.is_empty() {
        if args.json {
            println!("{}", status_json(&StatusCounts::default()));
        } else if args.quiet {
            println!("0");
        } else {
            println!("No sessions in profile '{}'.", storage.profile());
        }
        return Ok(());
    }

    crate::session::config::profile_config::resolve_config_or_warn(profile);

    crate::tmux::refresh_session_cache();

    let contended = crate::session::Instance::contended_capture_cwds(&instances);
    for inst in &mut instances {
        inst.update_status_once(None, None);
        inst.self_heal_session_id(profile, &contended);
    }

    let counts = count_statuses(instances.iter().map(|inst| inst.status));

    if args.json {
        println!("{}", status_json(&counts));
    } else if args.quiet {
        println!("{}", counts.waiting);
    } else if args.verbose {
        for (label, symbol, status) in VERBOSE_GROUPS {
            let rows: Vec<(String, String, String)> = instances
                .iter()
                .filter(|inst| inst.status == status)
                .map(|inst| {
                    (
                        inst.title.clone(),
                        inst.tool.clone(),
                        crate::util::collapse_tilde(&inst.project_path),
                    )
                })
                .collect();
            print!("{}", verbose_group(label, symbol, &rows));
        }
        println!(
            "Total: {} sessions in profile '{}'",
            counts.total,
            storage.profile()
        );
    } else if counts.stopped > 0 {
        println!(
            "{} waiting • {} running • {} idle • {} stopped",
            counts.waiting, counts.running, counts.idle, counts.stopped
        );
    } else {
        println!(
            "{} waiting • {} running • {} idle",
            counts.waiting, counts.running, counts.idle
        );
    }

    if !args.json && !args.quiet {
        crate::update::print_update_notice().await;
    }

    Ok(())
}
