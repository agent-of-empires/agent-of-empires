//! `aoe usage`: session usage from the local usage log.

use std::collections::HashMap;

use anyhow::Result;
use chrono::Utc;
use clap::{Args, Subcommand};
use serde::Serialize;

use crate::session::{Instance, Storage};
use crate::usage::{build_report, UsageEvent, UsageReport, UsageStore, UsageSummary};

#[derive(Args)]
#[command(args_conflicts_with_subcommands = true)]
pub struct UsageArgs {
    #[command(subcommand)]
    command: Option<UsageCommands>,
    /// Window to summarize, like `30d`, `12h`, or `90m`. Resumes are counted
    /// within the window.
    #[arg(long, default_value = "30d")]
    since: String,
    #[arg(long)]
    json: bool,
}

#[derive(Subcommand)]
pub enum UsageCommands {
    /// One session's usage summary and its latest events.
    Show {
        /// Session id, id prefix, or title.
        session: String,
        #[arg(long)]
        json: bool,
    },
    /// Raw usage events as JSON lines, oldest first (the Ledger feed).
    Export {
        /// Only events with a larger id.
        #[arg(long, default_value_t = 0)]
        after_id: i64,
        #[arg(long, default_value_t = 10_000)]
        limit: usize,
    },
}

pub async fn run(args: UsageArgs) -> Result<()> {
    let store = UsageStore::open_default()?;
    match args.command {
        None => report(&store, &args.since, args.json),
        Some(UsageCommands::Show { session, json }) => show(&store, &session, json),
        Some(UsageCommands::Export { after_id, limit }) => {
            for event in store.events_after(after_id, limit)? {
                println!("{}", serde_json::to_string(&event)?);
            }
            Ok(())
        }
    }
}

/// `30d`, `12h`, `90m` into a duration.
fn parse_since(value: &str) -> Result<chrono::Duration> {
    let err = || anyhow::anyhow!("--since must look like 30d, 12h, or 90m");
    let unit = value.chars().last().ok_or_else(err)?;
    let number = &value[..value.len() - unit.len_utf8()];
    let n: i64 = number.parse().map_err(|_| err())?;
    if n <= 0 {
        return Err(err());
    }
    // try_* rejects magnitudes that would overflow rather than panicking.
    let duration = match unit {
        'd' => chrono::Duration::try_days(n),
        'h' => chrono::Duration::try_hours(n),
        'm' => chrono::Duration::try_minutes(n),
        _ => return Err(err()),
    };
    duration.ok_or_else(err)
}

fn report(store: &UsageStore, since: &str, json: bool) -> Result<()> {
    let events = store.events_since(Utc::now() - parse_since(since)?)?;
    let report = build_report(&events);
    if json {
        return super::output::print_json(&report);
    }
    print_report(&report)
}

fn fmt_median(value: Option<f64>) -> String {
    value.map_or_else(|| "-".to_string(), |v| format!("{v:.1}"))
}

fn print_report(report: &UsageReport) -> Result<()> {
    println!("Instances:   {}", report.instances);
    println!("Contexts:    {}", report.contexts);
    println!("Clears:      {}", report.clears);
    println!(
        "Compactions: {} (auto {}, manual {})",
        report.compactions, report.compactions_auto, report.compactions_manual
    );
    println!("Resumes:     {}", report.resumes);
    println!("Prompts:     {}", report.prompts);
    println!(
        "Turns:       {} (errors {})",
        report.turns, report.turn_errors
    );
    let medians = [
        (
            "Median prompts/context:",
            fmt_median(report.median_prompts_per_context),
        ),
        (
            "Median context minutes:",
            fmt_median(report.median_context_minutes),
        ),
        (
            "Median resets/instance:",
            fmt_median(report.median_resets_per_instance),
        ),
        (
            "Max resets/instance:",
            report.max_resets_per_instance.to_string(),
        ),
    ];
    let label_width = medians
        .iter()
        .map(|(label, _)| label.len())
        .max()
        .unwrap_or(0);
    println!();
    for (label, value) in medians {
        println!("{label:<label_width$} {value}");
    }

    if !report.top.is_empty() {
        let titles = instance_titles();
        println!();
        println!("Top resets:");
        for (id, resets) in &report.top {
            let title = titles.get(id).map(String::as_str).unwrap_or(id);
            println!("  {resets:>3}  {title} ({})", super::truncate_id(id, 8));
        }
    }
    Ok(())
}

/// Every session across all profiles. A profile whose storage fails to open
/// or load is skipped rather than failing the whole listing.
fn all_instances() -> Result<Vec<Instance>> {
    let mut all = Vec::new();
    for profile in crate::session::list_profiles()? {
        let Ok(storage) = Storage::open_unwatched(&profile) else {
            continue;
        };
        let Ok((instances, _)) = storage.load_with_groups() else {
            continue;
        };
        all.extend(instances);
    }
    Ok(all)
}

/// Every session's title across all profiles, best-effort: any failure
/// (including `list_profiles` itself) leaves the map empty rather than
/// failing the whole report.
fn instance_titles() -> HashMap<String, String> {
    all_instances()
        .unwrap_or_default()
        .into_iter()
        .map(|inst| (inst.id, inst.title))
        .collect()
}

#[derive(Serialize)]
struct ShowJson<'a> {
    session: &'a str,
    title: &'a str,
    summary: &'a UsageSummary,
    events: &'a [UsageEvent],
}

fn show(store: &UsageStore, session: &str, json: bool) -> Result<()> {
    let instances = all_instances()?;
    let inst = super::resolve_session(session, &instances)?;
    let summary = store.summary_for(&inst.id)?;
    let events = store.events_for_instance(&inst.id)?;
    let start = events.len().saturating_sub(20);
    let events = &events[start..];

    if json {
        return super::output::print_json(&ShowJson {
            session: &inst.id,
            title: &inst.title,
            summary: &summary,
            events,
        });
    }

    println!("Session: {} ({})", inst.title, inst.id);
    println!("resets: {}", summary.resets);
    println!("  clears: {}", summary.clears);
    println!(
        "  compactions: {} (auto {}, manual {})",
        summary.compactions, summary.compactions_auto, summary.compactions_manual
    );
    println!("  resumes: {}", summary.resumes);
    println!("prompts: {}", summary.prompts);
    println!("turns: {} (errors {})", summary.turns, summary.turn_errors);
    if let Some(at) = summary.context_started_at {
        println!(
            "context started: {at} ({}p {}t)",
            summary.context_prompts, summary.context_turns
        );
    }
    if let Some(at) = summary.tracked_since {
        println!("tracked since: {at}");
    }
    if let Some(at) = summary.last_event_at {
        println!("last event: {at}");
    }
    println!();
    for event in events {
        println!(
            "{}  {}  {}  {}",
            event.occurred_at,
            event.kind.as_str(),
            event.detail.as_deref().unwrap_or("-"),
            event.agent.as_deref().unwrap_or("-"),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    fn parse_since_accepts_days_hours_and_minutes() {
        assert_eq!(parse_since("30d").unwrap(), chrono::Duration::days(30));
        assert_eq!(parse_since("12h").unwrap(), chrono::Duration::hours(12));
        assert_eq!(parse_since("90m").unwrap(), chrono::Duration::minutes(90));
        assert!(parse_since("x").is_err());
        assert!(parse_since("5w").is_err());
        assert!(parse_since("3д").is_err(), "multibyte unit must not panic");
        assert!(parse_since("").is_err());
        assert!(parse_since("0d").is_err(), "a zero window is meaningless");
        assert!(
            parse_since("-5d").is_err(),
            "a negative window is meaningless"
        );
        assert!(
            parse_since("99999999999999d").is_err(),
            "an overflowing window must not panic"
        );
    }

    #[test]
    #[serial]
    fn a_profile_with_an_unreadable_sessions_file_is_skipped_not_fatal() {
        let _guard = crate::session::test_support::isolate_app_dir();
        crate::session::create_profile("good").unwrap();
        crate::session::create_profile("broken").unwrap();

        Storage::open_unwatched("good")
            .unwrap()
            .update(|instances, _groups| {
                instances.push(Instance::new("keep-me", "/repo"));
                Ok(())
            })
            .unwrap();
        std::fs::write(
            crate::session::get_profile_dir_path("broken")
                .unwrap()
                .join("sessions.json"),
            "not json",
        )
        .unwrap();

        let instances = all_instances().expect("a broken sibling profile must not fail this");
        assert_eq!(instances.len(), 1);
        assert_eq!(instances[0].title, "keep-me");

        let titles = instance_titles();
        assert_eq!(titles.len(), 1);
        assert_eq!(
            titles.get(&instances[0].id).map(String::as_str),
            Some("keep-me")
        );
    }
}
