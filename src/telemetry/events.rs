//! Closed, versioned telemetry event schema: the whole wire payload is auditable here.

use std::collections::BTreeMap;

use serde::Serialize;

/// Bump on any wire shape change, including additive optional fields. v14 excludes trashed
/// sessions from every census, so v13 and v14 series must not be averaged together.
pub const SCHEMA_VERSION: u32 = 14;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Surface {
    Cli,
    Tui,
    Serve,
}

impl Surface {
    pub fn as_str(self) -> &'static str {
        match self {
            Surface::Cli => "cli",
            Surface::Tui => "tui",
            Surface::Serve => "serve",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ProcessStart {
    pub schema: u32,
    pub event: &'static str,
    /// Stable across redelivery so the gateway can dedup retried POSTs.
    pub uuid: String,
    pub install_id: String,
    pub sent_at: String,
    pub surface: Surface,
    pub aoe_version: String,
    pub os: String,
    pub arch: String,

    pub data_schema_version: u32,
    pub update_status: crate::update::UpdateStatus,
    pub update_releases_behind: crate::update::ReleasesBehind,
}

/// Emitted by short-lived CLI runs, at most once per install per day. Keys come from the
/// closed clap subcommand allowlist.
#[derive(Debug, Clone, Serialize)]
pub struct CliUsage {
    pub schema: u32,
    pub event: &'static str,
    pub install_id: String,
    pub sent_at: String,
    pub surface: Surface,
    pub aoe_version: String,
    pub os: String,
    pub arch: String,

    /// Window length varies, so aggregators can compute honest per-day rates.
    pub window_start: String,
    pub command_counts: BTreeMap<String, u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UsageSnapshot {
    pub schema: u32,
    pub event: &'static str,
    pub uuid: String,
    pub install_id: String,
    pub sent_at: String,
    pub surface: Surface,
    pub aoe_version: String,
    pub os: String,
    pub arch: String,

    pub data_schema_version: u32,
    pub update_status: crate::update::UpdateStatus,
    pub update_releases_behind: crate::update::ReleasesBehind,

    /// Excludes trashed sessions; the `sessions_by_*` maps partition this value.
    pub session_total: u32,
    pub session_running: u32,
    pub session_idle: u32,
    pub session_error: u32,
    pub session_structured: u32,
    pub session_sandboxed: u32,
    pub session_yolo: u32,

    /// `aoe serve` reports the window peak; the TUI reports the point-in-time total.
    pub peak_concurrent_sessions: u32,

    pub session_pinned: u32,
    pub session_snoozed: u32,
    pub session_archived: u32,

    /// Excluded from `session_total`; this is "currently in trash", not deletions.
    pub session_trashed: u32,

    pub sessions_by_agent: BTreeMap<String, u32>,
    pub sessions_by_model_bucket: BTreeMap<String, u32>,
    /// All five substrate keys are always present and partition `session_total`. Orthogonal to
    /// `session_sandboxed`: a sandboxed worktree counts as `worktree`.
    pub sessions_by_substrate: BTreeMap<String, u32>,

    /// Distinct sessions seen across the window, so the sum can exceed `session_total`.
    /// Trash applies at sample time. Serve-only.
    pub distinct_sessions_by_agent: BTreeMap<String, u32>,
    pub distinct_sessions_by_model_bucket: BTreeMap<String, u32>,

    pub features: BTreeMap<String, bool>,

    pub usage_seen: BTreeMap<String, u32>,

    /// Was-seen per client form factor, not a count. Empty on surfaces with no web client.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub web_clients_seen: BTreeMap<String, bool>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub structured_clients_seen: BTreeMap<String, bool>,

    pub session_creates_since_last_snapshot: u32,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_mode: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub serve_mode: Option<String>,

    /// The synthetic daemon-restart `Cancelled` decision is never counted.
    pub approvals_resolved: u32,
    pub approvals_by_decision: BTreeMap<String, u32>,
    pub agent_switches: u32,
    pub plan_mode_seen: bool,
    /// Reported by the browser, which alone owns the prompt queue.
    pub prompts_queued: u32,

    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub plugins_by_source: BTreeMap<String, u32>,
    /// Only builtin and featured plugins are named; others are counted by source only.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub plugins_active: BTreeMap<String, bool>,
}

#[derive(Debug, Clone, Default)]
pub struct StructuredInteractionCounts {
    pub approvals_allow: u32,
    pub approvals_allow_always: u32,
    pub approvals_deny: u32,
    pub agent_switches: u32,
    pub plan_mode_seen: bool,
    pub prompts_queued: u32,
}

impl StructuredInteractionCounts {
    pub fn approvals_resolved(&self) -> u32 {
        self.approvals_allow + self.approvals_allow_always + self.approvals_deny
    }

    pub fn approvals_by_decision(&self) -> BTreeMap<String, u32> {
        let mut map = BTreeMap::new();
        for (key, count) in [
            ("allow", self.approvals_allow),
            ("allow_always", self.approvals_allow_always),
            ("deny", self.approvals_deny),
        ] {
            if count > 0 {
                map.insert(key.to_string(), count);
            }
        }
        map
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approvals_resolved_sums_the_three_real_decisions() {
        let counts = StructuredInteractionCounts {
            approvals_allow: 2,
            approvals_allow_always: 1,
            approvals_deny: 1,
            ..Default::default()
        };
        assert_eq!(counts.approvals_resolved(), 4);
    }

    #[test]
    fn approvals_by_decision_omits_zero_keys() {
        let counts = StructuredInteractionCounts {
            approvals_allow: 2,
            approvals_deny: 1,
            ..Default::default()
        };
        let map = counts.approvals_by_decision();
        assert_eq!(map.get("allow"), Some(&2));
        assert_eq!(map.get("deny"), Some(&1));
        assert!(!map.contains_key("allow_always"));
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn empty_counts_produce_an_empty_decision_map() {
        let counts = StructuredInteractionCounts::default();
        assert_eq!(counts.approvals_resolved(), 0);
        assert!(counts.approvals_by_decision().is_empty());
    }
}
