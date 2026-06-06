//! `agent-of-empires status` command implementation

use anyhow::Result;
use clap::Args;
use serde::Serialize;

use crate::cli::target::{self, TargetFilter};
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

    /// Fleet selector. When any of `--all`, `--state`, `--errors`, `--group`,
    /// `--ids`, `--path`, or `--profiles` is present, status resolves the
    /// matched set (possibly across profiles), emits one row per session, and
    /// recomputes counts over exactly that set — the read-only counterpart to
    /// the bulk `send`/`restart` selectors. Unlike the mutating verbs, this
    /// includes archived/snoozed sessions and the invoking session.
    #[command(flatten)]
    filter: TargetFilter,
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

fn session_row(profile: &str, inst: &crate::session::Instance) -> StatusSessionJson {
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
}

fn build_session_rows(
    profile: &str,
    instances: &[crate::session::Instance],
) -> Vec<StatusSessionJson> {
    instances
        .iter()
        .map(|inst| session_row(profile, inst))
        .collect()
}

fn tally(counts: &mut StatusCounts, status: Status) {
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

#[tracing::instrument(target = "cli.session", skip_all, fields(profile = %profile))]
pub async fn run(profile: &str, args: StatusArgs) -> Result<()> {
    // Fleet-selector path: resolve the matched set (possibly cross-profile),
    // including archived/snoozed and the invoking session (read-only verb).
    if args.filter.has_selector() {
        return run_filtered(profile, args).await;
    }

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
        tally(&mut counts, inst.status);
    }
    counts
}

/// Sort priority for the filtered human table: most-actionable first
/// (running, then error, then waiting), idle/stopped last. Mirrors the
/// `fleet status` ordering so the native command reads the same.
fn state_order(status: Status) -> u8 {
    match status {
        Status::Running => 0,
        Status::Error => 1,
        Status::Waiting => 2,
        Status::Starting => 3,
        Status::Idle | Status::Unknown => 4,
        Status::Stopped => 5,
        Status::Deleting | Status::Creating => 8,
    }
}

/// Compact human age: `42s`, `7m`, `3h`, `5d`. `None` → `-`.
fn fmt_age(secs: Option<i64>) -> String {
    let Some(s) = secs else {
        return "-".to_string();
    };
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 86_400 {
        format!("{}h", s / 3600)
    } else {
        format!("{}d", s / 86_400)
    }
}

/// Read-only fleet-selector status: resolve the matched set (possibly across
/// profiles), then render JSON / quiet / human exactly like the unfiltered
/// path but scoped to that set with recomputed counts. `resolve_targets`
/// already refreshes the tmux cache and runs `update_status`, so states are
/// live without the extra pass the unfiltered path does.
async fn run_filtered(profile: &str, args: StatusArgs) -> Result<()> {
    // exclude_id = None (show self), mutating = false (include archived/snoozed).
    let targets = target::resolve_targets(profile, &args.filter, None, false)?;

    let mut counts = StatusCounts::default();
    for t in &targets {
        tally(&mut counts, t.instance.status);
    }

    if args.json {
        let sessions: Vec<StatusSessionJson> = targets
            .iter()
            .map(|t| session_row(&t.profile, &t.instance))
            .collect();
        let status_json = StatusJson {
            waiting: counts.waiting,
            running: counts.running,
            idle: counts.idle,
            stopped: counts.stopped,
            error: counts.error,
            total: counts.total,
            sessions,
        };
        println!("{}", serde_json::to_string(&status_json)?);
        return Ok(());
    }

    if args.quiet {
        println!("{}", counts.waiting);
        return Ok(());
    }

    if targets.is_empty() {
        println!("No sessions match the filter.");
        return Ok(());
    }

    let mut rows: Vec<&target::ResolvedTarget> = targets.iter().collect();
    rows.sort_by(|a, b| {
        state_order(a.instance.status)
            .cmp(&state_order(b.instance.status))
            .then_with(|| a.profile.cmp(&b.profile))
            .then_with(|| a.instance.title.cmp(&b.instance.title))
    });

    println!(
        "{} waiting • {} running • {} idle • {} stopped • {} error   (total {})",
        counts.waiting, counts.running, counts.idle, counts.stopped, counts.error, counts.total
    );
    for t in rows {
        let (age, _tool, _reason, urgent) = super::session_activity(&t.instance.id);
        let group = if t.instance.group_path.is_empty() {
            "-".to_string()
        } else {
            super::truncate(&t.instance.group_path, 14)
        };
        println!(
            "  {:<8} {:<13} {:<16} {:<14} {:>5} {}{}",
            t.instance.status.as_str(),
            t.profile,
            super::truncate_id(&t.instance.id, 16),
            group,
            fmt_age(age),
            t.instance.title,
            if urgent { "  !urgent" } else { "" },
        );
    }

    Ok(())
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
