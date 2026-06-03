//! Closed, versioned telemetry event schema.
//!
//! Both event kinds are plain serializable structs with a fixed set of
//! fields, so the entire wire payload is auditable from this file. There is
//! no open-ended map of arbitrary keys. Adding a field is a deliberate,
//! reviewable change; bump [`SCHEMA_VERSION`] when the shape changes.

use std::collections::BTreeMap;

use serde::Serialize;

/// Payload schema version. Bump on any change to the wire shape, including
/// additive optional fields, so a reader can tell which fields to expect.
///
/// v2 (#1941): added serve-only `auth_mode` / `serve_mode`.
/// v3 (#1886): added `sessions_by_substrate`, a mutually-exclusive
/// per-substrate census of live sessions.
/// v4 (#1931): added `session_pinned` / `session_snoozed` / `session_archived`.
/// v5 (#1880): replaced the `web_seen` / `cockpit_seen` booleans with the
/// allowlisted `usage_seen` count map.
/// v6 (#1933): added the [`CliUsage`] event and retired the `cli`-surface
/// [`ProcessStart`] in favor of it.
pub const SCHEMA_VERSION: u32 = 6;

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

/// Emitted once on boot by the long-running surfaces (TUI, `aoe serve`).
/// Captures launches that a periodic snapshot would miss. Carries no session
/// details. Short-lived `aoe <subcommand>` invocations no longer emit this;
/// they report through [`CliUsage`] instead, which carries the same
/// "this install ran the CLI" signal plus the per-command mix.
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

/// Emitted by short-lived `aoe <subcommand>` invocations, throttled to at most
/// once per install per day. Replaces the `cli`-surface [`ProcessStart`]: a
/// non-empty `command_counts` carries the same "this install ran the CLI today"
/// signal, plus which subcommands actually ran and how often.
///
/// Counts accumulate on disk across invocations (each `aoe` run is a separate
/// short-lived process, so there is no in-memory aggregation) and flush as one
/// POST per day. `command_counts` keys are the closed clap subcommand set
/// produced by [`crate::cli::command_name`]; they carry no args, flags, or
/// paths, and every key is filtered against the allowlist before sending, so
/// the event fits the closed-schema rule.
#[derive(Debug, Clone, Serialize)]
pub struct CliUsage {
    pub schema: u32,
    /// Always `"cli_usage"`.
    pub event: &'static str,
    pub install_id: String,
    pub sent_at: String,
    pub surface: Surface,
    pub aoe_version: String,
    pub os: String,
    pub arch: String,

    /// RFC 3339 UTC timestamp of the first command counted in this window. The
    /// window length varies (a user may run `aoe` once, then again days later),
    /// so this lets the aggregator compute honest per-day rates rather than
    /// assuming a fixed 24h window.
    pub window_start: String,
    /// Allowlisted clap subcommand name -> invocation count since the last
    /// confirmed flush (e.g. `{"add": 5, "list": 2}`).
    pub command_counts: BTreeMap<String, u32>,
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
    /// Primary, mutually-exclusive substrate bucket -> session count. Every
    /// session lands in exactly one of a fixed, closed vocabulary
    /// (`local` / `worktree` / `workspace` / `sandbox` / `scratch`), so the
    /// values partition `session_total` and always sum to it. All five keys
    /// are always present (pre-seeded to 0), so a dashboard never has to treat
    /// a missing key as zero. Keys are hardcoded, never derived from a path,
    /// repo name, or image string.
    ///
    /// This is orthogonal to `session_sandboxed`, which may overlap: a
    /// sandboxed worktree counts as `worktree` here (the substrate precedence
    /// puts worktree above sandbox) yet still increments `session_sandboxed`.
    /// So the `sandbox` bucket means "sandboxed and not also scratch /
    /// workspace / worktree", NOT "all sandboxed sessions"; use
    /// `session_sandboxed` for the latter.
    pub sessions_by_substrate: BTreeMap<String, u32>,
    /// Install-level feature adoption: allowlisted feature name -> active.
    /// Keyed by the fixed registry in [`super::features`]; lets new gated
    /// features be tracked by registering the flag, not by extending the
    /// schema. See `telemetry::features`.
    pub features: BTreeMap<String, bool>,

    /// Window-scoped usage activity: allowlisted signal name -> times the
    /// surface was opened since the last snapshot. Keyed by the fixed registry
    /// in [`super::usage_signals`]; instrumenting a new surface is one registry
    /// entry, not a schema field. Zero-valued keys stay present so the wire key
    /// set is stable. See `telemetry::usage_signals`.
    pub usage_seen: BTreeMap<String, u32>,

    /// Sessions created since the previous snapshot (a trend counter, not a
    /// per-session event stream).
    pub session_creates_since_last_snapshot: u32,

    /// Serve-only: how the daemon authenticates clients, decided once at
    /// launch. One of `"token"`, `"passphrase"`, `"none"`. `None` for the
    /// TUI / CLI surfaces, which host no server. Never carries the token or
    /// passphrase value, only the coarse mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_mode: Option<String>,

    /// Serve-only: how the daemon is exposed, decided once at launch. One of
    /// `"tunnel"` (Cloudflare quick or named), `"tailscale"` (Tailscale
    /// Funnel), or `"local"`. `None` for the TUI / CLI surfaces. Never carries
    /// a tunnel name, hostname, or `.ts.net` URL, only the coarse mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serve_mode: Option<String>,
}
