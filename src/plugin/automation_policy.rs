//! Host-owned policy for plugin-driven session automation (#2897):
//! approval-mode classification, rolling-window rate limits, active-session
//! concurrency limits, and the durable audit ledger backing them.
//!
//! Classification is security policy, so it never derives from agent- or
//! plugin-supplied metadata: the option catalog only proves a mode is
//! currently AVAILABLE, while the trusted table below (plus the adapter
//! profiles' bypass ids) decides what a mode is ALLOWED to do. Unknown modes
//! classify as unattended, fail closed.
//!
//! The ledger is a second, host-private [`crate::events`] schema inside the
//! existing `plugin_events.db`. It is never addressable from worker RPCs
//! (workers reach only the public event-bus schema), so a plugin cannot read
//! or forge policy records. Admissions are recorded before the operation
//! runs, which makes the rolling-hour limits survive daemon restarts; the
//! in-memory reservation map only closes same-process races and is
//! deliberately not persisted (a crashed daemon has no in-flight creates).

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

/// Hardcoded v1 limits, per plugin. Config exposure is a follow-up; these
/// are sized for cron-style automation (schedules plus retries) while
/// stopping rapid provisioning or prompt loops.
pub(crate) const MAX_PLUGIN_CREATES_PER_HOUR: u64 = 20;
pub(crate) const MAX_ACTIVE_PLUGIN_SESSIONS: usize = 5;
pub(crate) const MAX_PLUGIN_TURNS_PER_HOUR: u64 = 120;

const ROLLING_WINDOW_MS: i64 = 60 * 60 * 1000;
/// Per-topic ledger cap; at the admission limits above this retains far more
/// than the one-hour window the queries need.
const LEDGER_RETENTION_PER_TOPIC: usize = 2000;

/// Reviewed approval semantics for mode ids the host understands. The
/// less-restrictive entries (`default`, `plan`) are honored only for a reviewed
/// adapter (see [`classify_mode`]); an unreviewed agent reusing one of these ids
/// falls back to unattended. Everything else: the adapter profile's bypass id is
/// unattended, and an unknown id classifies unattended.
const TRUSTED_MODE_TABLE: &[(&str, ApprovalClass)] = &[
    // Adapter default approval-prompting presets.
    ("default", ApprovalClass::Interactive),
    // Claude's plan preset: read/analyze, edits still prompt.
    ("plan", ApprovalClass::Guarded),
    // Auto-writes files without a human approving each edit: unattended
    // behavior even though shell commands still prompt.
    ("acceptEdits", ApprovalClass::Unattended),
    // Adapter bypass ids (also covered by yolo_mode_id resolution).
    ("bypassPermissions", ApprovalClass::Unattended),
    ("agent-full-access", ApprovalClass::Unattended),
    ("yolo", ApprovalClass::Unattended),
];

/// Outcome of classifying a requested approval mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModeDecision {
    /// The mode is usable; enforce the class (unattended needs the grant).
    Class(ApprovalClass),
    /// The mode id is neither trusted-table-known nor advertised by the
    /// agent's discovered catalog.
    UnknownMode,
    /// An explicit mode was requested but the agent's catalog has never
    /// been discovered and the id is not independently known; refusing is
    /// safer than guessing.
    CatalogNotDiscovered,
}

/// Classify `mode_id` for `agent_key`. `catalog` is the agent's last
/// advertised option snapshot, `None` when never discovered.
///
/// The benign classifications (an omitted default, or the `default`/`plan`
/// table entries) describe the *reviewed* adapters' conventions. An unreviewed
/// agent (one with no static profile) could advertise a mode literally named
/// `default` or `plan`, or ship an omitted default that silently auto-applies,
/// so its less-restrictive treatment is not trusted: those cases fail closed to
/// unattended and thus require `session.unattended`. The always-unattended
/// entries and adapter bypass ids apply regardless.
pub(crate) fn classify_mode(
    agent_key: &str,
    mode_id: Option<&str>,
    catalog: Option<&AgentOptionEntry>,
) -> ModeDecision {
    let profile = crate::acp::agent_profiles::resolve(agent_key);
    // Only adapters with a reviewed static profile get the benign treatment;
    // anything falling back to DEFAULT fails closed.
    let reviewed = crate::acp::agent_profiles::is_reviewed(agent_key);

    let Some(mode_id) = mode_id else {
        // Omitted mode = the agent's own default. Trusted to prompt only for a
        // reviewed adapter; an unreviewed default could auto-apply, so fail
        // closed.
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
        // Honor a less-restrictive class only for a reviewed adapter; an
        // unreviewed agent reusing the id gets the fail-closed treatment.
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
        // Available but semantically unknown to the host: fail closed.
        ModeDecision::Class(ApprovalClass::Unattended)
    } else {
        ModeDecision::UnknownMode
    }
}

/// Durable admission ledger + process-local reservations. One per daemon,
/// owned by the plugin host.
pub struct AutomationPolicy {
    /// Ledger connection; sync mutex, tiny critical sections, no `await`
    /// while held (callers run queries via spawn_blocking-free short calls;
    /// the SQLite file is local and the tables are indexed by topic).
    ledger: std::sync::Mutex<Ledger>,
    /// Outstanding create reservations per plugin: creates admitted but not
    /// yet visible as persisted sessions, so concurrent different-key
    /// creates cannot overshoot the active-session limit.
    reservations: std::sync::Mutex<HashMap<String, usize>>,
}

struct Ledger {
    conn: Connection,
    schema: events::Schema,
}

impl AutomationPolicy {
    /// Open (creating on first use) the private audit schema inside the
    /// plugin event-bus database.
    pub(crate) fn open(plugin_events_db: &Path) -> Result<Self> {
        let schema = events::Schema::new("plugin_automation_audit")?;
        let conn = events::open(plugin_events_db, &schema)?;
        Ok(Self {
            ledger: std::sync::Mutex::new(Ledger { conn, schema }),
            reservations: std::sync::Mutex::new(HashMap::new()),
        })
    }

    /// Admit a `sessions.create`: enforce the rolling create rate and the
    /// active-session cap (persisted sessions + outstanding reservations),
    /// record the admission durably, and reserve a concurrency slot the
    /// caller must hold until its create resolves. `active_sessions` is the
    /// caller-supplied count of live (non-archived/snoozed/trashed) sessions
    /// this plugin already owns.
    pub(crate) fn admit_create(
        self: &std::sync::Arc<Self>,
        plugin_id: &str,
        active_sessions: usize,
    ) -> Result<CreateReservation, DispatchError> {
        // Reservation check-and-claim under one lock closes the
        // count-then-create race between concurrent different-key creates.
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
        // Rolling-window rate check + durable admission record. Admissions
        // are counted (not just successes) so failing requests cannot bypass
        // throttling; on denial the reservation drops with the error return.
        self.admit_windowed("create", plugin_id, MAX_PLUGIN_CREATES_PER_HOUR)?;
        Ok(reservation)
    }

    /// Admit a `sessions.turn.send` under the rolling turn rate.
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

    /// Record a policy decision or operation outcome in the audit ledger.
    /// Best-effort: auditing must never fail the operation itself. Never
    /// records prompt contents.
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

/// Holds one active-session concurrency slot for a create in flight;
/// released on drop on every exit path (success, error, panic).
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
    fn classification_table() {
        use ApprovalClass::*;
        use ModeDecision::*;
        let catalog = catalog_with_modes(&["default", "plan", "acceptEdits", "customMode"]);

        // Omitted mode: adapter default, interactive.
        assert_eq!(classify_mode("claude", None, None), Class(Interactive));
        // Adapter bypass id from the profile: unattended, catalog or not.
        assert_eq!(
            classify_mode("claude", Some("bypassPermissions"), None),
            Class(Unattended)
        );
        assert_eq!(
            classify_mode("codex", Some("agent-full-access"), None),
            Class(Unattended)
        );
        // Trusted table entries work without a discovered catalog.
        assert_eq!(classify_mode("claude", Some("plan"), None), Class(Guarded));
        assert_eq!(
            classify_mode("claude", Some("default"), None),
            Class(Interactive)
        );
        // acceptEdits auto-writes: unattended even though advertised.
        assert_eq!(
            classify_mode("claude", Some("acceptEdits"), Some(&catalog)),
            Class(Unattended)
        );
        // Advertised but unknown semantics: fail closed to unattended.
        assert_eq!(
            classify_mode("claude", Some("customMode"), Some(&catalog)),
            Class(Unattended)
        );
        // Not advertised, not known: invalid.
        assert_eq!(
            classify_mode("claude", Some("nope"), Some(&catalog)),
            UnknownMode
        );
        // Explicit unknown mode with no catalog yet: refuse rather than guess.
        assert_eq!(
            classify_mode("claude", Some("customMode"), None),
            CatalogNotDiscovered
        );
    }

    #[test]
    fn unreviewed_agent_fails_closed() {
        use ApprovalClass::*;
        use ModeDecision::*;

        // An unreviewed agent cannot inherit the benign classifications by
        // reusing a trusted id, nor by omitting the mode.
        assert_eq!(classify_mode("shady-agent", None, None), Class(Unattended));
        assert_eq!(
            classify_mode("shady-agent", Some("default"), None),
            Class(Unattended)
        );
        assert_eq!(
            classify_mode("shady-agent", Some("plan"), None),
            Class(Unattended)
        );
        // Always-unattended ids stay unattended for anyone.
        assert_eq!(
            classify_mode("shady-agent", Some("acceptEdits"), None),
            Class(Unattended)
        );
    }

    #[test]
    fn limits_and_reservations() {
        let dir = tempfile::tempdir().expect("tempdir");
        let policy = std::sync::Arc::new(
            AutomationPolicy::open(&dir.path().join("plugin_events.db")).expect("open"),
        );

        // Concurrency: active sessions + reservations are capped together.
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
        // Another plugin is unaffected.
        let _other = policy.admit_create("other", 0).expect("separate scope");
        // Releasing a slot re-admits.
        held.pop();
        let _again = policy.admit_create("cron", 0).expect("slot released");

        // Turn rate: durable rolling window.
        for _ in 0..MAX_PLUGIN_TURNS_PER_HOUR {
            policy.admit_turn("cron").expect("under the rate");
        }
        let denied = policy.admit_turn("cron").expect_err("rate reached");
        assert_eq!(denied.code, codes::RATE_LIMITED);
        assert_eq!(denied.data.as_ref().unwrap()["kind"], "rate_limited");
        policy.admit_turn("other").expect("separate scope");

        // The ledger survives a reopen (daemon restart): the window still
        // counts the prior admissions.
        drop(policy);
        let reopened = std::sync::Arc::new(
            AutomationPolicy::open(&dir.path().join("plugin_events.db")).expect("reopen"),
        );
        let denied = reopened.admit_turn("cron").expect_err("window persisted");
        assert_eq!(denied.data.as_ref().unwrap()["kind"], "rate_limited");
    }
}
