//! Closed, versioned telemetry event schema.
//!
//! Both event kinds are plain serializable structs with a fixed set of
//! fields, so the entire wire payload is auditable from this file. There is
//! no open-ended map of arbitrary keys. Adding a field is a deliberate,
//! reviewable change; bump [`SCHEMA_VERSION`] when the shape changes.

use std::collections::BTreeMap;

use serde::Serialize;

/// Payload schema version. Bump whenever the wire payload shape changes
/// (a new field, a removed field, a renamed field, or a type change); this is
/// a closed, auditable schema, so the version identifies the exact shape.
pub const SCHEMA_VERSION: u32 = 2;

/// Which surface emitted the event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Surface {
    /// A short-lived `aoe <subcommand>` invocation.
    Cli,
    /// The interactive terminal UI.
    Tui,
    /// The `aoe serve` daemon (web dashboard / cockpit host).
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

/// Emitted once on boot. Captures short-lived invocations that a periodic
/// snapshot would miss. Carries no session details.
#[derive(Debug, Clone, Serialize)]
pub struct ProcessStart {
    pub schema: u32,
    /// Always `"process_start"`.
    pub event: &'static str,
    pub install_id: String,
    /// RFC 3339 UTC timestamp.
    pub sent_at: String,
    pub surface: Surface,
    pub aoe_version: String,
    pub os: String,
    pub arch: String,
}

/// Emitted by long-running surfaces (TUI, `aoe serve`) on start, then every
/// ~12 hours, and best-effort on graceful shutdown. Carries current
/// aggregate state, never a per-action stream. Every string-valued bucket
/// has already passed through [`super::sanitize`].
#[derive(Debug, Clone, Serialize)]
pub struct UsageSnapshot {
    pub schema: u32,
    /// Always `"usage_snapshot"`.
    pub event: &'static str,
    pub install_id: String,
    pub sent_at: String,
    pub surface: Surface,
    pub aoe_version: String,
    pub os: String,
    pub arch: String,

    pub session_total: u32,
    pub session_running: u32,
    pub session_idle: u32,
    pub session_error: u32,
    pub session_cockpit: u32,
    pub session_sandboxed: u32,
    pub session_yolo: u32,

    /// Sessions currently pinned, snoozed (future `snoozed_until`), or
    /// archived at snapshot time. Point-in-time state prevalence, not action
    /// counts; the three are mutually exclusive per the session triage
    /// invariant, so they sum to at most `session_total`. Set through a shared
    /// mutator layer, so this census covers both the web and TUI surfaces with
    /// no per-surface wiring.
    pub session_pinned: u32,
    pub session_snoozed: u32,
    pub session_archived: u32,

    /// Allowlisted agent bucket -> session count.
    pub sessions_by_agent: BTreeMap<String, u32>,
    /// Coarse model family bucket -> session count.
    pub sessions_by_model_bucket: BTreeMap<String, u32>,
    /// Install-level feature adoption: allowlisted feature name -> active.
    /// Keyed by the fixed registry in [`super::features`]; lets new gated
    /// features be tracked by registering the flag, not by extending the
    /// schema. See `telemetry::features`.
    pub features: BTreeMap<String, bool>,

    /// The web dashboard was opened at least once since the last snapshot.
    pub web_seen: bool,
    /// The cockpit web UI was opened at least once since the last snapshot.
    pub cockpit_seen: bool,

    /// Sessions created since the previous snapshot (a trend counter, not a
    /// per-session event stream).
    pub session_creates_since_last_snapshot: u32,
}
