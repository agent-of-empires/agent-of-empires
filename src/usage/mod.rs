//! Local usage log: agent context resets, prompts, turns, and aoe lifecycle
//! events per instance, stored in `<app_dir>/usage.db`. The table and export
//! format are the Ledger contract in `docs/development/usage-events.md`.

mod normalize;
mod report;
mod store;
mod summary;

use std::path::PathBuf;

use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serialize;

pub use normalize::{normalize, Normalized};
pub use report::{build_report, UsageReport};
pub use store::UsageStore;
pub use summary::{is_context_boundary, summarize, UsageSummary};

const DB_FILE: &str = "usage.db";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageKind {
    ContextStart,
    ContextEnd,
    Compact,
    Prompt,
    TurnEnd,
    InstanceCreated,
    InstanceRestarted,
    InstanceDeleted,
}

impl UsageKind {
    const ALL: [UsageKind; 8] = [
        UsageKind::ContextStart,
        UsageKind::ContextEnd,
        UsageKind::Compact,
        UsageKind::Prompt,
        UsageKind::TurnEnd,
        UsageKind::InstanceCreated,
        UsageKind::InstanceRestarted,
        UsageKind::InstanceDeleted,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            UsageKind::ContextStart => "context_start",
            UsageKind::ContextEnd => "context_end",
            UsageKind::Compact => "compact",
            UsageKind::Prompt => "prompt",
            UsageKind::TurnEnd => "turn_end",
            UsageKind::InstanceCreated => "instance_created",
            UsageKind::InstanceRestarted => "instance_restarted",
            UsageKind::InstanceDeleted => "instance_deleted",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UsageEvent {
    /// Store-assigned and monotonic; 0 before insert.
    pub id: i64,
    #[serde(serialize_with = "serialize_occurred_at")]
    pub occurred_at: DateTime<Utc>,
    pub instance_id: String,
    pub profile: Option<String>,
    pub agent: Option<String>,
    pub kind: UsageKind,
    pub detail: Option<String>,
    pub agent_session_id: Option<String>,
}

/// RFC 3339 UTC with milliseconds, the contract's timestamp format
/// (`docs/development/usage-events.md`); chrono's default serde impl drops
/// the fraction on an exact second.
pub(crate) fn format_occurred_at(at: &DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn serialize_occurred_at<S: serde::Serializer>(
    at: &DateTime<Utc>,
    s: S,
) -> Result<S::Ok, S::Error> {
    s.serialize_str(&format_occurred_at(at))
}

pub fn db_path() -> anyhow::Result<PathBuf> {
    Ok(crate::session::get_app_dir()?.join(DB_FILE))
}

/// Best-effort: a lifecycle row never fails the operation that produced it.
pub fn record_lifecycle(instance: &crate::session::Instance, kind: UsageKind) {
    let enabled = crate::session::config::profile_config::resolve_config_or_warn(
        &instance.effective_profile(),
    )
    .session
    .usage_tracking;
    let result = db_path().and_then(|db| record_lifecycle_at(&db, instance, kind, enabled));
    if let Err(e) = result {
        tracing::debug!(target: "usage", "lifecycle event dropped: {e}");
    }
}

fn record_lifecycle_at(
    db: &std::path::Path,
    instance: &crate::session::Instance,
    kind: UsageKind,
    enabled: bool,
) -> anyhow::Result<()> {
    if !enabled {
        return Ok(());
    }
    UsageStore::open(db)?.insert(&UsageEvent {
        id: 0,
        occurred_at: Utc::now(),
        instance_id: instance.id.clone(),
        profile: Some(instance.effective_profile()),
        agent: Some(instance.tool.clone()),
        kind,
        detail: None,
        agent_session_id: instance.agent_session_id.clone(),
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The export contract promises milliseconds always; chrono's default
    /// serde impl drops the fraction on an exact second.
    #[test]
    fn occurred_at_serializes_with_milliseconds_on_an_exact_second() {
        use chrono::TimeZone;
        let event = UsageEvent {
            id: 1,
            occurred_at: Utc.with_ymd_and_hms(2026, 9, 29, 12, 0, 0).unwrap(),
            instance_id: "inst".into(),
            profile: None,
            agent: None,
            kind: UsageKind::Prompt,
            detail: None,
            agent_session_id: None,
        };
        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(value["occurred_at"], "2026-09-29T12:00:00.000Z");
    }

    #[test]
    fn kinds_round_trip_through_their_names() {
        for kind in UsageKind::ALL {
            assert_eq!(UsageKind::parse(kind.as_str()), Some(kind));
            assert_eq!(
                serde_json::to_value(kind).unwrap(),
                serde_json::Value::String(kind.as_str().to_string())
            );
        }
        assert_eq!(UsageKind::parse("nope"), None);
    }

    #[test]
    fn lifecycle_rows_carry_instance_identity_and_respect_the_switch() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("usage.db");
        let mut instance = crate::session::Instance::new("t", "/tmp/project");
        instance.tool = "claude".into();
        // Pin an explicit profile so this test doesn't race other tests mutating the
        // process-global default profile that `effective_profile()` falls back to.
        instance.source_profile = "lifecycle-busy-reservation".into();
        record_lifecycle_at(&db, &instance, UsageKind::InstanceCreated, true).unwrap();
        record_lifecycle_at(&db, &instance, UsageKind::InstanceDeleted, false).unwrap();
        let events = UsageStore::open(&db)
            .unwrap()
            .events_for_instance(&instance.id)
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, UsageKind::InstanceCreated);
        assert_eq!(events[0].agent.as_deref(), Some("claude"));
        assert_eq!(
            events[0].profile.as_deref(),
            Some("lifecycle-busy-reservation")
        );
    }
}
