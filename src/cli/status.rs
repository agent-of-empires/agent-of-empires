//! `agent-of-empires status` command implementation

use anyhow::Result;
use clap::Args;
use serde::Serialize;

use crate::session::{Status, Storage};

#[derive(Args)]
pub struct StatusArgs {
    /// Show detailed session list
    #[arg(short = 'v', long)]
    verbose: bool,

    /// Only output waiting count (for scripts)
    #[arg(short = 'q', long)]
    quiet: bool,

    /// Output as JSON
    #[arg(long)]
    json: bool,
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
    /// Complete per-session rows. Additive: the count fields above are kept
    /// for existing consumers, and each row carries the identity fields
    /// (`id/title/path/group/profile`) ALONGSIDE `state` and the activity
    /// metadata, so one `status --json` call replaces the old
    /// `list --json` ⋈ `status -v` join on path.
    sessions: Vec<StatusSessionJson>,
}

/// A complete per-session row in `status --json`: identity + live state +
/// activity metadata. This is the shape the fleet consumer keys on to address
/// matched sessions (send/restart) without a second call.
#[derive(Serialize)]
struct StatusSessionJson {
    id: String,
    title: String,
    path: String,
    group: String,
    profile: String,
    /// Live status word: `running`/`waiting`/`idle`/`stopped`/`error`/etc.
    state: String,
    /// Seconds since the session's last hook activity (`now - since`).
    #[serde(skip_serializing_if = "Option::is_none")]
    last_activity_age_secs: Option<i64>,
    /// Tool name on the most recent Pre/PostToolUse event (see `HookAttention`).
    #[serde(skip_serializing_if = "Option::is_none")]
    last_tool: Option<String>,
    /// Kind of the most recent hook event: `turn_complete`/`tool_invoke`/`tool_error`.
    #[serde(skip_serializing_if = "Option::is_none")]
    last_reason: Option<String>,
    /// Honored urgent flag (respects `urgent_expires_at`).
    urgent: bool,
}

fn build_session_rows(
    profile: &str,
    instances: &[crate::session::Instance],
) -> Vec<StatusSessionJson> {
    instances
        .iter()
        .map(|inst| {
            let (last_activity_age_secs, last_tool, last_reason, urgent) =
                super::session_activity(&inst.id);
            StatusSessionJson {
                id: inst.id.clone(),
                title: inst.title.clone(),
                path: inst.project_path.clone(),
                group: inst.group_path.clone(),
                profile: profile.to_string(),
                state: inst.status.as_str().to_string(),
                last_activity_age_secs,
                last_tool,
                last_reason,
                urgent,
            }
        })
        .collect()
}

#[tracing::instrument(target = "cli.session", skip_all, fields(profile = %profile))]
pub async fn run(profile: &str, args: StatusArgs) -> Result<()> {
    let storage = Storage::new(profile)?;
    let (mut instances, _) = storage.load_with_groups()?;

    if instances.is_empty() {
        if args.json {
            let empty = StatusJson {
                waiting: 0,
                running: 0,
                idle: 0,
                stopped: 0,
                error: 0,
                total: 0,
                sessions: Vec::new(),
            };
            println!("{}", serde_json::to_string(&empty)?);
        } else if args.quiet {
            println!("0");
        } else {
            println!("No sessions in profile '{}'.", storage.profile());
        }
        return Ok(());
    }

    // Refresh tmux session cache
    crate::tmux::refresh_session_cache();

    // Update status for all instances
    for inst in &mut instances {
        inst.update_status();
    }

    let counts = count_by_status(&instances);

    if args.json {
        let status_json = StatusJson {
            waiting: counts.waiting,
            running: counts.running,
            idle: counts.idle,
            stopped: counts.stopped,
            error: counts.error,
            total: counts.total,
            sessions: build_session_rows(storage.profile(), &instances),
        };
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

    // Show update notice if available (skip for JSON/quiet output)
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
        let path = shorten_path(&inst.project_path);
        println!("  {} {:<16} {:<10} {}", symbol, inst.title, inst.tool, path);
    }
    println!();
}

fn shorten_path(path: &str) -> String {
    if let Some(home) = dirs::home_dir() {
        if let Some(home_str) = home.to_str() {
            if let Some(stripped) = path.strip_prefix(home_str) {
                return format!("~{}", stripped);
            }
        }
    }
    path.to_string()
}
