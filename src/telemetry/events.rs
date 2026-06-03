//! Closed, versioned telemetry event schema.
//!
//! Both event kinds are plain serializable structs with a fixed set of
//! fields, so the entire wire payload is auditable from this file. There is
//! no open-ended map of arbitrary keys. Adding a field is a deliberate,
//! reviewable change; bump [`SCHEMA_VERSION`] when the shape changes.

use std::collections::BTreeMap;

use serde::Serialize;

/// Payload schema version. Bump on any breaking change to the field set.
///
/// v2 (#1883): the `usage_snapshot` gained the `web_clients_seen` /
/// `cockpit_clients_seen` per-form-factor maps alongside the coarse
/// `web_seen` / `cockpit_seen` flags.
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

    /// Coarse client form-factor classes that opened the web dashboard since
    /// the last snapshot: allowlisted class key (`desktop` / `desktop_pwa` /
    /// `mobile` / `mobile_pwa`) -> was-seen. A boolean, not a count, on the
    /// wire: it answers "which client classes used the dashboard" without
    /// leaking open frequency. Empty (and so omitted) on surfaces that host no
    /// web client, e.g. the TUI. See `telemetry::form_factor`.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub web_clients_seen: BTreeMap<String, bool>,
    /// Same per-class was-seen map for the cockpit web UI.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub cockpit_clients_seen: BTreeMap<String, bool>,

    /// Sessions created since the previous snapshot (a trend counter, not a
    /// per-session event stream).
    pub session_creates_since_last_snapshot: u32,
}
