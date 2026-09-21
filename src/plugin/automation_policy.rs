//! Host-owned policy for plugin-driven session automation (#2897):

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use rusqlite::Connection;

use aoe_plugin_api::acp::ApprovalClass;

use crate::acp::option_catalog::AgentOptionEntry;
use crate::acp::state::ConfigOptionCategory;
use crate::events;
use crate::plugin::host_api::DispatchError;
use crate::plugin::protocol::codes;

pub(crate) const MAX_PLUGIN_CREATES_PER_HOUR: u64 = 20;
pub(crate) const MAX_ACTIVE_PLUGIN_SESSIONS: usize = 5;
pub(crate) const MAX_PLUGIN_TURNS_PER_HOUR: u64 = 120;

const ROLLING_WINDOW_MS: i64 = 60 * 60 * 1000;
const LEDGER_RETENTION_PER_TOPIC: usize = 2000;

const TRUSTED_MODE_TABLE: &[(&str, ApprovalClass)] = &[
    ("default", ApprovalClass::Interactive),
    ("plan", ApprovalClass::Guarded),
    ("acceptEdits", ApprovalClass::Unattended),
    ("bypassPermissions", ApprovalClass::Unattended),
    ("agent-full-access", ApprovalClass::Unattended),
    ("yolo", ApprovalClass::Unattended),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModeDecision {
    Class(ApprovalClass),
    UnknownMode,
    CatalogNotDiscovered,
}

pub(crate) fn classify_mode(
    agent_key: &str,
    mode_id: Option<&str>,
    catalog: Option<&AgentOptionEntry>,
) -> ModeDecision {
    let profile = crate::acp::agent_profiles::resolve(agent_key);
    let reviewed = crate::acp::agent_profiles::is_reviewed(agent_key);

    let Some(mode_id) = mode_id else {
        return ModeDecision::Class(if reviewed {
            ApprovalClass::Interactive
        } else {
            ApprovalClass::Unattended
        });
    };
    if profile.yolo_mode_id == Some(mode_id) {
        return ModeDecision::Class(ApprovalClass::Unattended);
    }
    if let Some((_, class)) = TRUSTED_MODE_TABLE.iter().find(|(id, _)| *id == mode_id) {
        let effective = if *class == ApprovalClass::Unattended || reviewed {
            *class
        } else {
            ApprovalClass::Unattended
        };
        return ModeDecision::Class(effective);
    }
    let Some(catalog) = catalog else {
        return ModeDecision::CatalogNotDiscovered;
    };
    let advertised = catalog.options.iter().any(|opt| {
        opt.category == ConfigOptionCategory::Mode
            && opt.options.iter().any(|choice| choice.value == mode_id)
    });
    if advertised {
        ModeDecision::Class(ApprovalClass::Unattended)
    } else {
        ModeDecision::UnknownMode
    }
}

pub struct AutomationPolicy {
    ledger: std::sync::Mutex<Ledger>,
    reservations: std::sync::Mutex<HashMap<String, usize>>,
}

struct Ledger {
    conn: Connection,
    schema: events::Schema,
}

impl AutomationPolicy {
    pub(crate) fn open(plugin_events_db: &Path) -> Result<Self> {
        let schema = events::Schema::new("plugin_automation_audit")?;
        let conn = events::open(plugin_events_db, &schema)?;
        Ok(Self {
            ledger: std::sync::Mutex::new(Ledger { conn, schema }),
            reservations: std::sync::Mutex::new(HashMap::new()),
        })
    }

    pub(crate) fn admit_create(
        self: &std::sync::Arc<Self>,
        plugin_id: &str,
        active_sessions: usize,
    ) -> Result<CreateReservation, DispatchError> {
        {
            let mut reservations = self
                .reservations
                .lock()
                .expect("reservations mutex poisoned");
            let outstanding = reservations.get(plugin_id).copied().unwrap_or(0);
            if active_sessions + outstanding >= MAX_ACTIVE_PLUGIN_SESSIONS {
                return Err(DispatchError::with_kind(
                    codes::RATE_LIMITED,
                    "concurrency_limited",
                    format!(
                        "plugin {plugin_id} already has {} active or pending sessions (limit {MAX_ACTIVE_PLUGIN_SESSIONS})",
                        active_sessions + outstanding
                    ),
                ));
            }
            *reservations.entry(plugin_id.to_string()).or_insert(0) += 1;
        }
        let reservation = CreateReservation {
            policy: std::sync::Arc::clone(self),
            plugin_id: plugin_id.to_string(),
        };
        self.admit_windowed("create", plugin_id, MAX_PLUGIN_CREATES_PER_HOUR)?;
        Ok(reservation)
    }

    pub(crate) fn admit_turn(&self, plugin_id: &str) -> Result<(), DispatchError> {
        self.admit_windowed("turn", plugin_id, MAX_PLUGIN_TURNS_PER_HOUR)
    }

    fn admit_windowed(
        &self,
        operation: &str,
        plugin_id: &str,
        limit: u64,
    ) -> Result<(), DispatchError> {
        let topic = format!("{operation}/{plugin_id}");
        let now = chrono::Utc::now().timestamp_millis();
        let ledger = self.ledger.lock().expect("ledger mutex poisoned");
        let used = events::count_since(
            &ledger.conn,
            &ledger.schema,
            &topic,
            now - ROLLING_WINDOW_MS,
        )
        .map_err(|e| DispatchError::internal(format!("automation ledger read failed: {e:#}")))?;
        if used >= limit {
            return Err(DispatchError::with_kind(
                codes::RATE_LIMITED,
                "rate_limited",
                format!(
                    "plugin {plugin_id} exceeded {limit} admitted {operation} operations in the rolling hour"
                ),
            ));
        }
        let seq = events::highest_seq(&ledger.conn, &ledger.schema, &topic) + 1;
        let payload = serde_json::json!({ "op": operation, "plugin": plugin_id }).to_string();
        events::insert_event(&ledger.conn, &ledger.schema, &topic, seq, &payload, now).map_err(
            |e| DispatchError::internal(format!("automation ledger write failed: {e:#}")),
        )?;
        events::prune_retention(
            &ledger.conn,
            &ledger.schema,
            &topic,
            LEDGER_RETENTION_PER_TOPIC,
            &[],
        );
        Ok(())
    }

    pub(crate) fn audit(&self, plugin_id: &str, record: serde_json::Value) {
        let topic = format!("decision/{plugin_id}");
        let now = chrono::Utc::now().timestamp_millis();
        let ledger = self.ledger.lock().expect("ledger mutex poisoned");
        let seq = events::highest_seq(&ledger.conn, &ledger.schema, &topic) + 1;
        if let Err(e) = events::insert_event(
            &ledger.conn,
            &ledger.schema,
            &topic,
            seq,
            &record.to_string(),
            now,
        ) {
            tracing::warn!(
                target: "plugin.automation",
                plugin = %plugin_id,
                "audit record write failed: {e:#}"
            );
        }
        events::prune_retention(
            &ledger.conn,
            &ledger.schema,
            &topic,
            LEDGER_RETENTION_PER_TOPIC,
            &[],
        );
    }

    fn release_reservation(&self, plugin_id: &str) {
        let mut reservations = self
            .reservations
            .lock()
            .expect("reservations mutex poisoned");
        if let Some(count) = reservations.get_mut(plugin_id) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                reservations.remove(plugin_id);
            }
        }
    }
}

pub(crate) struct CreateReservation {
    policy: std::sync::Arc<AutomationPolicy>,
    plugin_id: String,
}

impl Drop for CreateReservation {
    fn drop(&mut self) {
        self.policy.release_reservation(&self.plugin_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::state::{ConfigOptionChoice, ConfigOptionDescriptor};

    fn catalog_with_modes(modes: &[&str]) -> AgentOptionEntry {
        AgentOptionEntry {
            updated_at: "2026-07-16T00:00:00Z".to_string(),
            options: vec![ConfigOptionDescriptor {
                id: "mode".to_string(),
                name: "Mode".to_string(),
                description: None,
                category: ConfigOptionCategory::Mode,
                current_value: String::new(),
                options: modes
                    .iter()
                    .map(|m| ConfigOptionChoice {
                        value: (*m).to_string(),
                        name: (*m).to_string(),
                        description: None,
                    })
                    .collect(),
            }],
        }
    }

    #[test]
    fn mode_classification_table() {
        use ApprovalClass::*;
        use ModeDecision::*;
        let catalog = catalog_with_modes(&["default", "plan", "acceptEdits", "customMode"]);

        // agent, mode, catalog discovered, decision
        let cases = [
            ("claude", None, false, Class(Interactive)),
            ("claude", Some("default"), false, Class(Interactive)),
            ("claude", Some("plan"), false, Class(Guarded)),
            (
                "claude",
                Some("bypassPermissions"),
                false,
                Class(Unattended),
            ),
            ("codex", Some("agent-full-access"), false, Class(Unattended)),
            ("claude", Some("acceptEdits"), true, Class(Unattended)),
            ("claude", Some("customMode"), true, Class(Unattended)),
            ("claude", Some("nope"), true, UnknownMode),
            ("claude", Some("customMode"), false, CatalogNotDiscovered),
            // An agent nobody reviewed fails closed in every mode.
            ("shady-agent", None, false, Class(Unattended)),
            ("shady-agent", Some("default"), false, Class(Unattended)),
            ("shady-agent", Some("plan"), false, Class(Unattended)),
            ("shady-agent", Some("acceptEdits"), false, Class(Unattended)),
        ];
        for (agent, mode, discovered, expected) in cases {
            let seen = classify_mode(agent, mode, discovered.then_some(&catalog));
            assert_eq!(seen, expected, "{agent} {mode:?} discovered={discovered}");
        }
    }
    #[test]
    fn limits_and_reservations() {
        let dir = tempfile::tempdir().expect("tempdir");
        let policy = std::sync::Arc::new(
            AutomationPolicy::open(&dir.path().join("plugin_events.db")).expect("open"),
        );

        let mut held = Vec::new();
        for _ in 0..MAX_ACTIVE_PLUGIN_SESSIONS {
            held.push(
                policy
                    .admit_create("cron", 0)
                    .map_err(|e| e.message)
                    .expect("under the cap"),
            );
        }
        let denied = match policy.admit_create("cron", 0) {
            Err(e) => e,
            Ok(_) => panic!("cap reached"),
        };
        assert_eq!(denied.code, codes::RATE_LIMITED);
        assert_eq!(denied.data.as_ref().unwrap()["kind"], "concurrency_limited");
        let _other = policy.admit_create("other", 0).expect("separate scope");
        held.pop();
        let _again = policy.admit_create("cron", 0).expect("slot released");

        for _ in 0..MAX_PLUGIN_TURNS_PER_HOUR {
            policy.admit_turn("cron").expect("under the rate");
        }
        let denied = policy.admit_turn("cron").expect_err("rate reached");
        assert_eq!(denied.code, codes::RATE_LIMITED);
        assert_eq!(denied.data.as_ref().unwrap()["kind"], "rate_limited");
        policy.admit_turn("other").expect("separate scope");

        drop(policy);
        let reopened = std::sync::Arc::new(
            AutomationPolicy::open(&dir.path().join("plugin_events.db")).expect("reopen"),
        );
        let denied = reopened.admit_turn("cron").expect_err("window persisted");
        assert_eq!(denied.data.as_ref().unwrap()["kind"], "rate_limited");
    }
}
