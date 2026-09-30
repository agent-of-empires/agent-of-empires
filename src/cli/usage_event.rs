//! Hidden `aoe __usage-event` subcommand: records one agent hook event in the
//! usage log. Spawned by host hooks and the Pi extension; always exits 0.

use std::io::Read;
use std::path::Path;

use anyhow::{anyhow, Result};
use clap::Args;

use super::hook_input::{fired_by_pane_agent, publisher_is_pane_agent, read_json};

#[derive(Args)]
pub struct UsageEventArgs {
    /// Agent that fired the hook; defaults to `AOE_AGENT_BIN`.
    #[arg(long)]
    agent: Option<String>,
}

pub async fn run(args: UsageEventArgs) -> Result<()> {
    let Ok(instance_id) = std::env::var("AOE_INSTANCE_ID") else {
        return Ok(());
    };
    if let Err(e) = crate::session::validate_instance_id(&instance_id) {
        tracing::debug!(target: "usage", "rejecting unsafe AOE_INSTANCE_ID: {e}");
        return Ok(());
    }
    if !publisher_is_pane_agent(
        args.agent.as_deref(),
        std::env::var("AOE_AGENT_BIN").ok().as_deref(),
    ) || !fired_by_pane_agent()
    {
        tracing::debug!(target: "usage", "ignoring hook from a nested agent process");
        return Ok(());
    }
    let env = |key: &str| std::env::var(key).ok().filter(|value| !value.is_empty());
    let agent = args.agent.or_else(|| env("AOE_AGENT_BIN"));
    let profile = env("AOE_PROFILE");
    let result = crate::usage::db_path().and_then(|db| {
        record(
            std::io::stdin().lock(),
            &db,
            &instance_id,
            agent.as_deref(),
            profile.as_deref(),
        )
    });
    if let Err(e) = result {
        tracing::debug!(target: "usage", "usage event dropped: {e}");
    }
    Ok(())
}

/// Returns whether a row was written; an event the log does not track is not an error.
fn record<R: Read>(
    stdin: R,
    db: &Path,
    instance_id: &str,
    agent: Option<&str>,
    profile: Option<&str>,
) -> Result<bool> {
    let payload = read_json(stdin)?;
    let Some(normalized) = crate::usage::normalize(agent.unwrap_or(""), &payload) else {
        return Ok(false);
    };
    let event = crate::usage::UsageEvent {
        id: 0,
        occurred_at: chrono::Utc::now(),
        instance_id: instance_id.to_string(),
        profile: profile.map(str::to_string),
        agent: agent.map(str::to_string),
        kind: normalized.kind,
        detail: normalized.detail,
        agent_session_id: normalized.agent_session_id,
    };
    crate::usage::UsageStore::open(db)
        .and_then(|store| store.insert(&event))
        .map_err(|e| anyhow!("insert failed: {e}"))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_tracked_events_only() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("usage.db");
        let cases: [(&str, Option<&str>, bool); 4] = [
            (
                r#"{"hook_event_name":"SessionStart","source":"clear","session_id":"s1"}"#,
                Some("claude"),
                true,
            ),
            (r#"{"hook_event_name":"PreToolUse"}"#, Some("claude"), false),
            (
                r#"{"hook_event_name":"session_compact","reason":"threshold"}"#,
                Some("pi"),
                true,
            ),
            ("not json", Some("claude"), false),
        ];
        for (payload, agent, written) in cases {
            let got = record(payload.as_bytes(), &db, "inst", agent, Some("work"));
            assert_eq!(got.unwrap_or(false), written, "{payload}");
        }
        let events = crate::usage::UsageStore::open(&db)
            .unwrap()
            .events_for_instance("inst")
            .unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].agent.as_deref(), Some("claude"));
        assert_eq!(events[0].profile.as_deref(), Some("work"));
        assert_eq!(events[0].detail.as_deref(), Some("clear"));
        assert_eq!(events[0].agent_session_id.as_deref(), Some("s1"));
        assert_eq!(events[1].detail.as_deref(), Some("auto"));
    }
}
