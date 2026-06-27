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
/// v5 (#1880): replaced the `web_seen` / `acp_seen` booleans with the
/// allowlisted `usage_seen` count map.
/// v6 (#1933): added the [`CliUsage`] event and retired the `cli`-surface
/// [`ProcessStart`] in favor of it.
/// v7 (#1887): added version-health fields (`data_schema_version`,
/// `update_status`, `update_releases_behind`) to every event.
/// v8 (#1873): added a per-event `uuid` idempotency key to `process_start`
/// and `usage_snapshot`.
/// v9 (#1883): added the `web_clients_seen` / `structured_clients_seen`
/// per-form-factor maps alongside the `usage_seen` open counts.
/// v10 (#1870): added serve windowed fields `peak_concurrent_sessions` and the
/// `distinct_sessions_by_agent` / `distinct_sessions_by_model_bucket` maps.
/// v11 (#1888): added the structured-interaction aggregates (`approvals_resolved`,
/// `approvals_by_decision`, `agent_switches`, `view_toggles`,
/// `plan_mode_seen`, `prompts_queued`).
/// v12: retired `view_toggles`. The terminal<->structured swap it counted has no
/// trigger in the shipped UI (structured view is the default since #1925), so it
/// only ever reported zero. The `acp_enable` / `acp_disable` endpoints stay; they
/// just no longer feed a dead metric.
/// v13 (#2367): added the plugin census `plugins_by_source` (installed count per
/// source bucket) and `plugins_active` (active state for builtin + featured ids
/// only; unfeatured GitHub and local installs are counted by source but never
/// named).
pub const SCHEMA_VERSION: u32 = 13;

/// Which surface emitted the event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Surface {
    /// A short-lived `aoe <subcommand>` invocation.
    Cli,
    /// The interactive terminal UI.
    Tui,
    /// The `aoe serve` daemon (web dashboard / acp host).
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
    /// Random v4 UUID minted once when the event is built. Stable across any
    /// redelivery of the same logical event, so the gateway can forward it as
    /// the PostHog event `uuid` and let PostHog dedup retried POSTs. Distinct
    /// from `install_id` (stable per install) and `sent_at` (per-emit stamp).
    pub uuid: String,
    pub install_id: String,
    /// RFC 3339 UTC timestamp.
    pub sent_at: String,
    pub surface: Surface,
    pub aoe_version: String,
    pub os: String,
    pub arch: String,

    /// On-disk data-schema version this build targets (`migrations::CURRENT_VERSION`).
    /// A small integer, never a version string.
    pub data_schema_version: u32,
    /// Coarse update-staleness by semver distance, from the cached update check.
    pub update_status: crate::update::UpdateStatus,
    /// Coarse "how many releases behind", counted from the cached release list.
    pub update_releases_behind: crate::update::ReleasesBehind,
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
/// ~4 hours, and best-effort on graceful shutdown. Carries current
/// aggregate state, never a per-action stream. Every string-valued bucket
/// has already passed through [`super::sanitize`].
#[derive(Debug, Clone, Serialize)]
pub struct UsageSnapshot {
    pub schema: u32,
    /// Always `"usage_snapshot"`.
    pub event: &'static str,
    /// Random v4 UUID minted once when the event is built; see
    /// [`ProcessStart::uuid`]. Excluded from the in-process dedup fingerprint
    /// (`super::snapshot_fingerprint`) so two snapshots with identical content
    /// still compare equal.
    pub uuid: String,
    pub install_id: String,
    pub sent_at: String,
    pub surface: Surface,
    pub aoe_version: String,
    pub os: String,
    pub arch: String,

    /// On-disk data-schema version this build targets (`migrations::CURRENT_VERSION`).
    /// A small integer, never a version string.
    pub data_schema_version: u32,
    /// Coarse update-staleness by semver distance, from the cached update check.
    pub update_status: crate::update::UpdateStatus,
    /// Coarse "how many releases behind", counted from the cached release list.
    pub update_releases_behind: crate::update::ReleasesBehind,

    pub session_total: u32,
    pub session_running: u32,
    pub session_idle: u32,
    pub session_error: u32,
    pub session_structured: u32,
    pub session_sandboxed: u32,
    pub session_yolo: u32,

    /// Peak concurrent `session_total` observed across the window since the
    /// last snapshot. `aoe serve` folds a sample every ~30 min into a local
    /// aggregate and reports the max here, so a short-lived burst of sessions
    /// that opens and closes between two 4h sends is still captured. The TUI
    /// does not aggregate, so it reports the point-in-time `session_total`.
    pub peak_concurrent_sessions: u32,

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

    /// Allowlisted agent bucket -> distinct sessions *seen* across the window
    /// since the last snapshot (not concurrent at the tick), so short-lived
    /// sessions caught by a ~30-min sample still contribute their agent mix.
    /// The sum can exceed `session_total`. Populated by `aoe serve`; empty on
    /// the TUI, which does not aggregate.
    pub distinct_sessions_by_agent: BTreeMap<String, u32>,
    /// Coarse model family bucket -> distinct sessions seen across the window.
    /// Same window semantics as [`Self::distinct_sessions_by_agent`]; serve-only.
    pub distinct_sessions_by_model_bucket: BTreeMap<String, u32>,

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

    /// Coarse client form-factor classes that opened the web dashboard since
    /// the last snapshot: allowlisted class key (`desktop` / `desktop_pwa` /
    /// `mobile` / `mobile_pwa`) -> was-seen. A boolean, not a count, on the
    /// wire: it answers "which client classes used the dashboard" without
    /// leaking open frequency. Empty (and so omitted) on surfaces that host no
    /// web client, e.g. the TUI. See `telemetry::form_factor`.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub web_clients_seen: BTreeMap<String, bool>,
    /// Same per-class was-seen map for the acp web UI.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub structured_clients_seen: BTreeMap<String, bool>,

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

    /// Structured view approvals the user resolved since the last snapshot. The sum of
    /// [`Self::approvals_by_decision`]; the synthetic daemon-restart
    /// `Cancelled` decision is never counted (it is not a user choice).
    pub approvals_resolved: u32,
    /// Decision mix for resolved approvals: allowlisted decision key
    /// (`allow` / `allow_always` / `deny`) -> count. Zero-count keys are
    /// omitted, like [`Self::sessions_by_agent`]. Shows whether people lean on
    /// `allow_always` (trust) vs `deny` (friction).
    pub approvals_by_decision: BTreeMap<String, u32>,
    /// Mid-session agent switches since the last snapshot.
    pub agent_switches: u32,
    /// A session entered plan mode at least once since the last snapshot.
    pub plan_mode_seen: bool,
    /// Prompts the web structured view queued (parked because the agent was busy)
    /// since the last snapshot. Reported by the browser, the only surface that
    /// owns the prompt queue; the daemon never sees the queue directly.
    pub prompts_queued: u32,

    /// Installed-plugin census: source bucket (`builtin` / `featured` /
    /// `community` / `local`) -> count. Counts the whole installed population
    /// per source, no identity, so it is safe for every source. Empty (and so
    /// omitted) when no plugins are loaded, e.g. a TUI-only build with no
    /// installs. See `telemetry::plugins`.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub plugins_by_source: BTreeMap<String, u32>,
    /// Active-state for the plugins whose identity is safe to name: builtin
    /// (compiled in) and featured (in the curated index). Allowlisted id ->
    /// active. Unfeatured GitHub (possibly private) and local installs are
    /// counted in [`Self::plugins_by_source`] but never named here, per the
    /// telemetry privacy rule. Empty (and so omitted) when no nameable plugin
    /// is loaded. See `telemetry::plugins`.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub plugins_active: BTreeMap<String, bool>,
}

/// Resolved structured-interaction counts for one snapshot window, the input the
/// daemon folds into a [`UsageSnapshot`]. Surfaces without a structured view (the TUI)
/// pass [`Default`] (all zero). Counts only, plus a closed decision key set, so
/// nothing here can carry free-form content past [`super::sanitize`].
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
    /// Total user-resolved approvals (the three real decisions; `Cancelled`
    /// is never accumulated here).
    pub fn approvals_resolved(&self) -> u32 {
        self.approvals_allow + self.approvals_allow_always + self.approvals_deny
    }

    /// Decision mix as an allowlisted-key map, omitting zero counts to match
    /// the `sessions_by_agent` convention.
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
        // Issue #1888 story: 2 allow + 1 deny (+ 1 allow_always here) all count.
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
        // Matches the sessions_by_agent convention: absent key == zero.
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
