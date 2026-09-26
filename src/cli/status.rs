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
struct StatusCounts {
    running: usize,
    waiting: usize,
    idle: usize,
    stopped: usize,
    error: usize,
    total: usize,
}

#[derive(Serialize)]
struct StatusJson {
    waiting: usize,
    running: usize,
    idle: usize,
    stopped: usize,
    error: usize,
    total: usize,
}

/// One spelling for both JSON branches. The empty-profile answer used to be a
/// hand-written literal while the populated one was serialized, so the same
/// command emitted different bytes depending on whether the profile held a
/// session.
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
#[tracing::instrument(target = "cli.session", skip_all, fields(profile = %profile))]
pub async fn run(profile: &str, args: StatusArgs) -> Result<()> {
    let storage = Storage::open_unwatched(profile)?;
    let (mut instances, _) = storage.load_with_groups()?;
    for inst in &mut instances {
        inst.source_profile = storage.profile().to_string();
    }

    if instances.is_empty() {
        if args.json {
            println!(
                "{}",
                serde_json::to_string(&StatusJson::from(&StatusCounts::default()))?
            );
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

    let counts = count_by_status(&instances);

    if args.json {
        let status_json = StatusJson::from(&counts);
        println!("{}", serde_json::to_string(&status_json)?);
    } else if args.quiet {
        println!("{}", counts.waiting);
    } else if args.verbose {
        print_status_group("WAITING", "⠃", Status::Waiting, &instances);
        print_status_group("RUNNING", "⠋", Status::Running, &instances);
        print_status_group("IDLE", "⠒", Status::Idle, &instances);
        print_status_group("STOPPED", "⠒", Status::Stopped, &instances);
        print_status_group("ERROR", "✕", Status::Error, &instances);
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

fn count_by_status(instances: &[crate::session::Instance]) -> StatusCounts {
    let mut counts = StatusCounts::default();
    for inst in instances {
        match inst.status {
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

fn print_status_group(
    label: &str,
    symbol: &str,
    status: Status,
    instances: &[crate::session::Instance],
) {
    let matching: Vec<_> = instances.iter().filter(|i| i.status == status).collect();
    if matching.is_empty() {
        return;
    }

    println!("{} ({}):", label, matching.len());
    for inst in matching {
        let path = crate::util::collapse_tilde(&inst.project_path);
        println!("  {} {:<16} {:<10} {}", symbol, inst.title, inst.tool, path);
    }
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The empty-profile answer is the same bytes the daemon read emits for the
    /// same profile, so `--json` is one shape whatever the transport and
    /// whatever the profile holds.
    #[test]
    fn the_empty_profile_json_is_the_same_compact_bytes_everywhere() {
        let empty =
            serde_json::to_string(&StatusJson::from(&StatusCounts::default())).expect("serializes");
        assert_eq!(
            empty,
            r#"{"waiting":0,"running":0,"idle":0,"stopped":0,"error":0,"total":0}"#
        );
        let counts = StatusCounts {
            waiting: 1,
            ..StatusCounts::default()
        };
        let populated = serde_json::to_string(&StatusJson::from(&counts)).expect("serializes");
        assert_eq!(
            populated,
            r#"{"waiting":1,"running":0,"idle":0,"stopped":0,"error":0,"total":0}"#
        );
    }
}
