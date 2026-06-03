//! Anonymous, opt-in usage telemetry.
//!
//! Design constraints (see issue #1762):
//! - **Off by default.** Nothing is sent unless the user opts in via
//!   [`crate::session::TelemetryConfig::enabled`] in any settings surface.
//! - **`DO_NOT_TRACK` is absolute.** When set (`1` / `true` / `yes`), it
//!   suppresses both sending and install-id generation regardless of config.
//! - **Endpoint.** Opted-in sends go to the collection gateway at
//!   [`DEFAULT_ENDPOINT`] (which validates and re-sanitizes as a backstop);
//!   `AOE_TELEMETRY_ENDPOINT` overrides it, e.g. to point at a local sink. A
//!   compiled-in [`TELEMETRY_KEY`] is sent as `X-Telemetry-Key` so the gateway
//!   can shed drive-by noise (it is visible in source, so not real auth).
//! - **Fire-and-forget.** Sends run detached with a hard timeout (plus a short
//!   connect timeout so a down endpoint fails fast) and swallow every error
//!   (logged only at `debug`, `target: "telemetry"`). Telemetry must never
//!   slow, stall, or crash the tool.
//! - **Sanitized.** No content ever leaves [`sanitize`]: agent/model strings
//!   are coerced to a closed allowlist; raw commands, paths, titles, branch
//!   names, and prompts are never emitted.

pub mod events;
pub mod features;
pub mod sanitize;
mod state;
pub mod usage_signals;

use std::collections::BTreeMap;
use std::time::Duration;

pub use events::{CliUsage, ProcessStart, Surface, UsageSnapshot, SCHEMA_VERSION};
pub use state::{
    cli_usage_due, ensure_install_id, install_id, record_cli_command, record_cli_usage_flush,
    reset_install_id,
};

use crate::session::Instance;

/// Hard cap on any single telemetry send. Both the reqwest client timeout and
/// the outer flush bound use it, so a dead or slow endpoint can never delay
/// the CLI's exit or a daemon tick beyond this.
const SEND_TIMEOUT: Duration = Duration::from_secs(2);

/// Connect timeout for the send. Much shorter than [`SEND_TIMEOUT`] so a
/// black-holed or slow-DNS endpoint fails in well under a second rather than
/// costing a CLI run the full send budget.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);

/// Default collection gateway. Overridable via `AOE_TELEMETRY_ENDPOINT` (handy
/// for pointing at a local sink to inspect what is sent). The gateway
/// validates the envelope and re-sanitizes every field as a defense-in-depth
/// backstop. Nothing reaches it unless the user has opted in.
const DEFAULT_ENDPOINT: &str = "https://telemetry.agent-of-empires.com/v1/ingest";

/// Static key sent as `X-Telemetry-Key`. NOT authentication: it is visible in
/// this source, so it only lets the gateway drop unkeyed drive-by traffic. The
/// gateway must be configured to require this exact value.
const TELEMETRY_KEY: &str = "7bc5a4e45ce861662b9690a7105da988";

/// CLI `cli_usage` is the only *high-frequency* event source in normal use (one
/// per `aoe` invocation, and users script `aoe` in loops), so its flush is
/// throttled locally to at most once per install per day. Per-command counts
/// accumulate on disk between flushes, so a single daily POST still answers
/// "which commands did this install run" without a POST per command. TUI and
/// `aoe serve` `process_start` stay per-launch and are deliberately not capped:
/// one emit per launch is the signal we want, and suppressing it would hide
/// legitimate restarts. A pathological crash-loop could still flood from those
/// surfaces; that is accepted as a telemetry-only risk, absorbed by the
/// gateway's `X-Telemetry-Key` rate limiting rather than a local throttle.
const CLI_USAGE_MIN_GAP: Duration = Duration::from_secs(24 * 60 * 60);

/// Retry backoff after a *failed* CLI `cli_usage` send. While the daily slot
/// stays open (a failed send never claims it), this bounds re-attempts to once
/// per hour so a down endpoint can't make every `aoe` invocation re-send.
const CLI_USAGE_RETRY_GAP: Duration = Duration::from_secs(60 * 60);

/// Base cadence for periodic `usage_snapshot` sends (TUI and serve). The real
/// period is this plus bounded jitter (see [`snapshot_interval`]).
pub const SNAPSHOT_BASE_INTERVAL: Duration = Duration::from_secs(12 * 60 * 60);

/// Upper bound on the random jitter added to [`SNAPSHOT_BASE_INTERVAL`].
const SNAPSHOT_JITTER: Duration = Duration::from_secs(30 * 60);

/// Periodic snapshot period: [`SNAPSHOT_BASE_INTERVAL`] plus a random offset in
/// `[0, SNAPSHOT_JITTER)`. A fixed 12h period anchored to process start means a
/// fleet that boots together (e.g. a post-update restart wave) keeps snapshotting
/// in lockstep forever; rolling a per-process jitter decorrelates the periodic
/// ticks so they spread apart by the second tick. The boot snapshot is sent
/// separately and stays immediate, so this only shapes the steady-state cadence.
pub fn snapshot_interval() -> Duration {
    use rand::RngExt;
    let jitter_ms = rand::rng().random_range(0..SNAPSHOT_JITTER.as_millis() as u64);
    SNAPSHOT_BASE_INTERVAL + Duration::from_millis(jitter_ms)
}

/// True when `DO_NOT_TRACK` is set to an affirmative value. This is the
/// absolute override: it wins over `config.telemetry.enabled`.
pub fn do_not_track() -> bool {
    match std::env::var("DO_NOT_TRACK") {
        Ok(v) => {
            let v = v.trim().to_ascii_lowercase();
            matches!(v.as_str(), "1" | "true" | "yes")
        }
        Err(_) => false,
    }
}

/// The send endpoint. `AOE_TELEMETRY_ENDPOINT` overrides when set to a
/// non-empty value; otherwise the compiled-in [`DEFAULT_ENDPOINT`] is used.
/// Always returns a target, so the opt-in gate (not a missing endpoint) is
/// what decides whether anything is sent.
pub fn endpoint() -> String {
    match std::env::var("AOE_TELEMETRY_ENDPOINT") {
        Ok(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => DEFAULT_ENDPOINT.to_string(),
    }
}

/// Consent state, ignoring whether a backend is wired. True when the user has
/// opted in and `DO_NOT_TRACK` is not suppressing. Drives id generation and
/// whether events are built at all.
pub fn is_opted_in() -> bool {
    crate::session::get_telemetry_settings().enabled && !do_not_track()
}

/// Opt-in check against an already-loaded `Config`, so a caller that needs the
/// full config anyway (e.g. [`build_usage_snapshot`] for `active_features`)
/// doesn't parse `config.toml` a second time via [`is_opted_in`].
fn opted_in_with(config: &crate::session::Config) -> bool {
    config.telemetry.enabled && !do_not_track()
}

/// Apply an opt-in/opt-out transition's side effect on the install id. The
/// caller is responsible for persisting `config.telemetry.enabled`; this only
/// manages `telemetry.json`. Enabling (when not suppressed) generates the id;
/// disabling deletes it. Centralised so every surface (CLI, TUI, web, consent
/// prompts) behaves identically.
pub fn apply_opt_in_change(enabled: bool) {
    if enabled {
        if !do_not_track() {
            let _ = state::ensure_install_id();
        }
    } else if let Err(e) = state::delete_install_id() {
        tracing::debug!(target: "telemetry", "failed to delete install id on opt-out: {e}");
    }
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// The fixed, closed substrate vocabulary, in precedence order. A session is
/// classified into exactly one of these by [`substrate_bucket`]; the snapshot
/// pre-seeds all five so the census is always complete.
const SUBSTRATES: [&str; 5] = ["scratch", "workspace", "worktree", "sandbox", "local"];

/// Classify a session into its single primary substrate bucket.
///
/// Mutually exclusive by fixed precedence: `scratch` > `workspace` >
/// `worktree` > `sandbox` > `local`. `scratch` is invariant-exclusive with
/// worktree/workspace, so a session carrying both is an upstream state bug; we
/// log it at `debug` and bucket by precedence rather than panic, because
/// telemetry must never crash the tool. A sandbox can legitimately co-occur
/// with a worktree, so it sits below worktree and the orthogonal
/// `session_sandboxed` count carries the "has sandbox at all" signal.
fn substrate_bucket(inst: &Instance) -> &'static str {
    let has_worktree = inst.worktree_info.is_some();
    let has_workspace = inst.workspace_info.is_some();
    if inst.scratch {
        if has_worktree || has_workspace {
            tracing::debug!(
                target: "telemetry",
                has_worktree,
                has_workspace,
                "scratch session also carries worktree/workspace info; bucketing as scratch by precedence"
            );
        }
        return "scratch";
    }
    if has_workspace {
        return "workspace";
    }
    if has_worktree {
        return "worktree";
    }
    if inst.sandbox_info.as_ref().is_some_and(|s| s.enabled) {
        return "sandbox";
    }
    "local"
}

/// Build a `process_start` event, or `None` when telemetry is not opted in
/// (or `DO_NOT_TRACK` suppresses id generation). Emitted only by the long-lived
/// surfaces (TUI / serve); short-lived CLI runs report via `cli_usage` instead.
pub fn build_process_start(surface: Surface) -> Option<ProcessStart> {
    if !is_opted_in() {
        return None;
    }
    let install_id = state::ensure_install_id()?;
    Some(ProcessStart {
        schema: SCHEMA_VERSION,
        event: "process_start",
        install_id,
        sent_at: now_rfc3339(),
        surface,
        aoe_version: env!("CARGO_PKG_VERSION").to_string(),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
    })
}

/// Pure per-session aggregation, split out of [`build_usage_snapshot`] so the
/// counting logic is unit-testable without the opt-in / install-id / config
/// global state the snapshot builder pulls in.
struct InstanceMetrics {
    total: u32,
    running: u32,
    idle: u32,
    error: u32,
    cockpit: u32,
    sandboxed: u32,
    yolo: u32,
    pinned: u32,
    snoozed: u32,
    archived: u32,
    by_agent: BTreeMap<String, u32>,
    by_model_bucket: BTreeMap<String, u32>,
    by_substrate: BTreeMap<String, u32>,
}

fn aggregate_instances(instances: &[Instance]) -> InstanceMetrics {
    let mut by_agent: BTreeMap<String, u32> = BTreeMap::new();
    let mut by_model_bucket: BTreeMap<String, u32> = BTreeMap::new();
    // Pre-seed every substrate to 0 so the census is always complete: a
    // dashboard never has to coalesce a missing key, and the values always
    // sum to `session_total`.
    let mut by_substrate: BTreeMap<String, u32> =
        SUBSTRATES.iter().map(|s| (s.to_string(), 0)).collect();
    let (mut running, mut idle, mut error, mut cockpit, mut sandboxed, mut yolo) =
        (0u32, 0u32, 0u32, 0u32, 0u32, 0u32);
    let (mut pinned, mut snoozed, mut archived) = (0u32, 0u32, 0u32);

    for inst in instances {
        match inst.status {
            crate::session::Status::Running => running += 1,
            crate::session::Status::Idle => idle += 1,
            crate::session::Status::Error => error += 1,
            _ => {}
        }
        // Cockpit fields only exist in `serve` builds; treat them as absent
        // otherwise so the aggregation stays surface-agnostic.
        #[cfg(feature = "serve")]
        let is_cockpit = inst.cockpit_mode;
        #[cfg(not(feature = "serve"))]
        let is_cockpit = false;
        if is_cockpit {
            cockpit += 1;
        }
        if inst.sandbox_info.as_ref().is_some_and(|s| s.enabled) {
            sandboxed += 1;
        }
        if inst.yolo_mode {
            yolo += 1;
        }

        // Mutually-exclusive primary substrate; orthogonal to the sandbox count
        // above (a sandboxed worktree buckets as `worktree` here). The map is
        // pre-seeded with the closed vocabulary, so increment the existing key
        // rather than inserting: any drift in `substrate_bucket` then fails
        // loudly instead of silently broadening the payload.
        *by_substrate
            .get_mut(substrate_bucket(inst))
            .expect("SUBSTRATES must contain every substrate bucket") += 1;

        // Point-in-time session-triage census. The three states are mutually
        // exclusive per the triage invariant enforced in the session apply /
        // merge path (see `Instance::archive`/`snooze`/`pin` and the merge
        // reconciliation), so independent checks never double-count a
        // well-formed session. The debug assert makes a future mutator or
        // merge regression fail fast instead of silently skewing the census
        // (sum of the three counts exceeding `session_total`).
        let is_pinned = inst.is_pinned();
        let is_snoozed = inst.is_snoozed();
        let is_archived = inst.is_archived();
        debug_assert!(
            [is_pinned, is_snoozed, is_archived]
                .into_iter()
                .filter(|state| *state)
                .count()
                <= 1,
            "session triage states must be mutually exclusive"
        );
        if is_pinned {
            pinned += 1;
        }
        if is_snoozed {
            snoozed += 1;
        }
        if is_archived {
            archived += 1;
        }

        // Prefer the canonical detection name; fall back to the raw tool
        // string. Either way it is coerced to an allowlisted bucket.
        let agent_src = if inst.detect_as.trim().is_empty() {
            inst.tool.as_str()
        } else {
            inst.detect_as.as_str()
        };
        *by_agent
            .entry(sanitize::agent_bucket(agent_src))
            .or_insert(0) += 1;

        #[cfg(feature = "serve")]
        let model = inst.cockpit_model.as_deref();
        #[cfg(not(feature = "serve"))]
        let model: Option<&str> = None;
        let bucket = sanitize::model_bucket(model);
        *by_model_bucket.entry(bucket.to_string()).or_insert(0) += 1;
    }

    InstanceMetrics {
        total: instances.len() as u32,
        running,
        idle,
        error,
        cockpit,
        sandboxed,
        yolo,
        pinned,
        snoozed,
        archived,
        by_agent,
        by_model_bucket,
        by_substrate,
    }
}

/// Build a `usage_snapshot` from the current sessions, or `None` when not
/// opted in. All agent/model strings pass through [`sanitize`]; raw values
/// never reach the payload.
pub fn build_usage_snapshot(
    surface: Surface,
    instances: &[Instance],
    usage_seen: BTreeMap<String, u32>,
    session_creates_since_last_snapshot: u32,
    auth_mode: Option<&str>,
    serve_mode: Option<&str>,
) -> Option<UsageSnapshot> {
    // Load the global, pre-profile-merge config exactly once and reuse it for
    // both the opt-in gate and `active_features`, instead of parsing
    // `config.toml` twice (once via `is_opted_in`, once for features). It is the
    // install-level config on purpose: `features` is a default-adoption signal,
    // not per-session usage. See `features::active_features`.
    let config = crate::session::Config::load_or_warn();
    if !opted_in_with(&config) {
        return None;
    }
    // auth_mode / serve_mode are serve-only deployment metadata. Normalize here
    // rather than trusting every caller to pass None, so a future non-serve call
    // site can never leak them onto a TUI / CLI payload.
    debug_assert!(
        matches!(surface, Surface::Serve) || (auth_mode.is_none() && serve_mode.is_none()),
        "auth_mode and serve_mode are serve-only fields"
    );
    let (auth_mode, serve_mode) = if matches!(surface, Surface::Serve) {
        (auth_mode, serve_mode)
    } else {
        (None, None)
    };
    let install_id = state::ensure_install_id()?;
    let mut snapshot = assemble_usage_snapshot(
        surface,
        install_id,
        &config,
        instances,
        usage_seen,
        session_creates_since_last_snapshot,
    );
    // Layer the serve-only deployment metadata on top of the pure snapshot, so
    // `assemble_usage_snapshot` stays focused on session/feature bucketing.
    snapshot.auth_mode = auth_mode.map(str::to_string);
    snapshot.serve_mode = serve_mode.map(str::to_string);
    Some(snapshot)
}

/// Pure assembly of a `usage_snapshot` from an already-resolved install id and
/// config: no disk reads, no opt-in gate, no id generation. Split out of
/// [`build_usage_snapshot`] so the bucketing and feature-map logic can be unit
/// tested with an injected `Config` and no filesystem or env mutation.
fn assemble_usage_snapshot(
    surface: Surface,
    install_id: String,
    config: &crate::session::Config,
    instances: &[Instance],
    usage_seen: BTreeMap<String, u32>,
    session_creates_since_last_snapshot: u32,
) -> UsageSnapshot {
    let features = features::active_features(config);

    let metrics = aggregate_instances(instances);

    UsageSnapshot {
        schema: SCHEMA_VERSION,
        event: "usage_snapshot",
        install_id,
        sent_at: now_rfc3339(),
        surface,
        aoe_version: env!("CARGO_PKG_VERSION").to_string(),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        session_total: metrics.total,
        session_running: metrics.running,
        session_idle: metrics.idle,
        session_error: metrics.error,
        session_cockpit: metrics.cockpit,
        session_sandboxed: metrics.sandboxed,
        session_yolo: metrics.yolo,
        session_pinned: metrics.pinned,
        session_snoozed: metrics.snoozed,
        session_archived: metrics.archived,
        sessions_by_agent: metrics.by_agent,
        sessions_by_model_bucket: metrics.by_model_bucket,
        sessions_by_substrate: metrics.by_substrate,
        features,
        usage_seen,
        session_creates_since_last_snapshot,
        // Set by `build_usage_snapshot` for the serve surface; the pure
        // assembler leaves them unset.
        auth_mode: None,
        serve_mode: None,
    }
}

/// POST a serialized event to the endpoint. Returns `true` only on a *confirmed*
/// delivery: a transport-level `Ok` whose HTTP status is a 2xx. A transport error
/// OR a non-success status (4xx/5xx, e.g. a rejected `X-Telemetry-Key` or a
/// schema rejection at the gateway) returns `false` so callers can defer
/// consuming a signal until delivery is actually confirmed. Every error is
/// swallowed and logged at `debug` only. Bounded by both a short connect timeout
/// and the overall [`SEND_TIMEOUT`] so a down endpoint can never delay the caller.
async fn post<T: serde::Serialize>(event: &T) -> bool {
    let endpoint = endpoint();
    let client = match reqwest::Client::builder()
        .user_agent(concat!("agent-of-empires/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(SEND_TIMEOUT)
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!(target: "telemetry", "failed to build client: {e}");
            return false;
        }
    };
    match client
        .post(&endpoint)
        .header("X-Telemetry-Key", TELEMETRY_KEY)
        .json(event)
        .send()
        .await
    {
        Ok(resp) => {
            let status = resp.status();
            let ok = status.is_success();
            tracing::debug!(target: "telemetry", status = %status, ok, "telemetry send completed");
            ok
        }
        Err(e) => {
            tracing::debug!(target: "telemetry", "telemetry send failed: {e}");
            false
        }
    }
}

/// Emit a `process_start` for a long-running surface (TUI / serve). Detached:
/// returns immediately and never blocks the caller.
pub fn spawn_process_start(surface: Surface) {
    if let Some(event) = build_process_start(surface) {
        tokio::spawn(async move {
            post(&event).await;
        });
    }
}

/// Build a `cli_usage` event from the accumulated per-command counts, or `None`
/// when not opted in or there is nothing to report. Every key is filtered
/// against the closed [`crate::cli::CLI_COMMAND_NAMES`] allowlist, so a
/// hand-edited or corrupt `telemetry.json` can never smuggle an arbitrary
/// string onto the wire: the in-process recorder only ever writes allowlisted
/// names, and this is the defense-in-depth re-check before sending.
pub fn build_cli_usage() -> Option<CliUsage> {
    if !is_opted_in() {
        return None;
    }
    let (counts, window_start) = state::cli_usage_window();
    let command_counts: BTreeMap<String, u32> = counts
        .into_iter()
        .filter(|(name, _)| crate::cli::CLI_COMMAND_NAMES.contains(&name.as_str()))
        .collect();
    if command_counts.is_empty() {
        return None;
    }
    let install_id = state::ensure_install_id()?;
    let window_start = window_start
        .map(|w| w.to_rfc3339())
        .unwrap_or_else(now_rfc3339);
    Some(CliUsage {
        schema: SCHEMA_VERSION,
        event: "cli_usage",
        install_id,
        sent_at: now_rfc3339(),
        surface: Surface::Cli,
        aoe_version: env!("CARGO_PKG_VERSION").to_string(),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        window_start,
        command_counts,
    })
}

/// Record one CLI subcommand invocation and flush the accumulated `cli_usage`
/// event if a send is due. Called once per `aoe <subcommand>` run.
///
/// Side-effect-free unless the install is opted in: the [`app_dir_exists`] gate
/// is a non-creating check, so app-data-free commands (`aoe completion`,
/// `aoe init`, ...) on a not-opted-in install never materialize the app dir and
/// keep working in read-only / sandboxed environments. The daily slot is claimed
/// only after a *confirmed* send, so a failed send leaves the counts and the slot
/// intact for the next invocation to retry (bounded by [`CLI_USAGE_RETRY_GAP`]).
/// Awaited with a hard timeout so a dead endpoint can never hang the CLI's exit.
pub async fn track_cli_command(name: &str) {
    // Cheap non-creating gate first: opt-in creates the app dir, so its absence
    // means the install cannot be opted in, and we must not create it here.
    if !crate::session::app_dir_exists() || !is_opted_in() {
        return;
    }
    // Always record the command; this is a lock-protected RMW that also opens
    // the window on the first command since the last flush.
    state::record_cli_command(name);
    // Throttle the flush to the daily slot (with a bounded retry while a send
    // keeps failing); the per-command counts keep accumulating on disk until
    // a send is confirmed.
    if !cli_usage_due(CLI_USAGE_MIN_GAP, CLI_USAGE_RETRY_GAP) {
        return;
    }
    let Some(event) = build_cli_usage() else {
        return;
    };
    let confirmed = matches!(
        tokio::time::timeout(SEND_TIMEOUT, post(&event)).await,
        Ok(true)
    );
    // Stamp the attempt always; on a confirmed 2xx also claim the daily slot and
    // clear the reported counts/window. A failed send leaves both intact to retry.
    record_cli_usage_flush(confirmed);
}

/// Fingerprint of the last `usage_snapshot` whose send we initiated this
/// process. Lets [`flush_snapshot_if_changed`] drop a redundant exit snapshot
/// that would otherwise repeat the boot (or last periodic) snapshot verbatim
/// within seconds. Process-local on purpose: a fresh launch starts empty, which
/// is correct because `process_start` already carries the per-launch signal, so
/// the snapshot only needs to report state and identical state is not worth
/// re-sending back to back.
static LAST_SNAPSHOT_FP: std::sync::Mutex<Option<u64>> = std::sync::Mutex::new(None);

/// Content fingerprint of a snapshot, excluding the volatile `sent_at` stamp.
/// Everything else is included: `install_id` is stable per install, so two
/// snapshots with the same counts hash equal. Used only for in-process dedup,
/// never sent anywhere.
fn snapshot_fingerprint(snapshot: &UsageSnapshot) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut probe = snapshot.clone();
    probe.sent_at = String::new();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    serde_json::to_string(&probe)
        .unwrap_or_default()
        .hash(&mut hasher);
    hasher.finish()
}

/// Record that we just initiated a send for `snapshot`, so a later
/// [`flush_snapshot_if_changed`] can tell whether anything changed since.
fn record_snapshot_fp(snapshot: &UsageSnapshot) {
    if let Ok(mut last) = LAST_SNAPSHOT_FP.lock() {
        *last = Some(snapshot_fingerprint(snapshot));
    }
}

/// True when `snapshot` is identical (ignoring `sent_at`) to the last one whose
/// send we *confirmed* this process. Pure peek, no mutation: the fingerprint is
/// recorded by [`send_snapshot`] only after a confirmed send, so a failed send
/// never poisons the dedup cache into dropping a later identical retry. A
/// poisoned lock reports "not a duplicate", so sending is the safe default.
fn snapshot_matches_last(snapshot: &UsageSnapshot) -> bool {
    let fp = snapshot_fingerprint(snapshot);
    match LAST_SNAPSHOT_FP.lock() {
        Ok(last) => *last == Some(fp),
        Err(_) => false,
    }
}

/// Outcome of a snapshot flush, so a caller can decide whether to consume the
/// state the snapshot reported (e.g. the `usage_seen` counts / a create counter).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendOutcome {
    /// Delivery was confirmed (a 2xx). Safe to consume the reported state.
    Sent,
    /// Skipped because identical to the last confirmed send. The prior send
    /// already consumed the reported state; consume nothing again.
    Deduped,
    /// The send failed. Retain the reported state so the next snapshot retries.
    Failed,
}

/// Send a pre-built usage snapshot, awaiting delivery with a hard timeout.
/// Records the dedup fingerprint *only on a confirmed send*, so a failed send
/// never suppresses a later identical retry. Returns whether delivery was
/// confirmed. Caller builds via [`build_usage_snapshot`] (returns `None` when
/// not opted in).
pub async fn send_snapshot(snapshot: UsageSnapshot) -> bool {
    let confirmed = matches!(
        tokio::time::timeout(SEND_TIMEOUT, post(&snapshot)).await,
        Ok(true)
    );
    if confirmed {
        record_snapshot_fp(&snapshot);
    }
    confirmed
}

/// Send a pre-built usage snapshot, detached. Returns immediately and never
/// blocks the caller; the fingerprint is recorded inside the spawned task only
/// on a confirmed send.
pub fn spawn_snapshot(snapshot: UsageSnapshot) {
    tokio::spawn(async move {
        send_snapshot(snapshot).await;
    });
}

/// Send the best-effort snapshot on graceful exit, awaiting delivery with a
/// hard timeout so the final snapshot can flush without risking a hang, but
/// skipping the send when the snapshot is identical (ignoring `sent_at`) to the
/// last one already confirmed this run. A boot (or periodic) snapshot followed
/// by a quit with unchanged session state would otherwise post the same counts
/// twice within seconds; a snapshot that actually changed still flushes. The
/// returned [`SendOutcome`] lets the caller consume reported state only when the
/// send was actually confirmed.
pub async fn flush_snapshot_if_changed(snapshot: UsageSnapshot) -> SendOutcome {
    if snapshot_matches_last(&snapshot) {
        tracing::debug!(target: "telemetry", "exit snapshot unchanged since last confirmed emit; skipping duplicate");
        return SendOutcome::Deduped;
    }
    if send_snapshot(snapshot).await {
        SendOutcome::Sent
    } else {
        SendOutcome::Failed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    // `#[serial]` (the default global group) serializes these env-mutating
    // tests against every other telemetry test that reads `DO_NOT_TRACK` /
    // `AOE_TELEMETRY_ENDPOINT`, including the consent-dialog tests in another
    // module, so none of them race on the shared process env.
    #[test]
    #[serial]
    fn do_not_track_recognises_affirmative_values() {
        for v in ["1", "true", "TRUE", "yes", "Yes"] {
            unsafe { std::env::set_var("DO_NOT_TRACK", v) };
            assert!(do_not_track(), "{v} should suppress");
        }
        for v in ["0", "false", "no", ""] {
            unsafe { std::env::set_var("DO_NOT_TRACK", v) };
            assert!(!do_not_track(), "{v} should not suppress");
        }
        unsafe { std::env::remove_var("DO_NOT_TRACK") };
        assert!(!do_not_track());
    }

    #[test]
    #[serial]
    fn endpoint_falls_back_to_default_and_env_overrides() {
        // Unset or blank => the compiled-in default gateway.
        unsafe { std::env::remove_var("AOE_TELEMETRY_ENDPOINT") };
        assert_eq!(endpoint(), DEFAULT_ENDPOINT);
        unsafe { std::env::set_var("AOE_TELEMETRY_ENDPOINT", "   ") };
        assert_eq!(endpoint(), DEFAULT_ENDPOINT);
        // A non-empty value overrides (trimmed).
        unsafe { std::env::set_var("AOE_TELEMETRY_ENDPOINT", " https://x/y ") };
        assert_eq!(endpoint(), "https://x/y");
        unsafe { std::env::remove_var("AOE_TELEMETRY_ENDPOINT") };
    }

    fn sample_snapshot() -> UsageSnapshot {
        UsageSnapshot {
            schema: SCHEMA_VERSION,
            event: "usage_snapshot",
            install_id: "00000000-0000-0000-0000-000000000000".to_string(),
            sent_at: "2026-06-02T19:00:45Z".to_string(),
            surface: Surface::Tui,
            aoe_version: "0.0.0".to_string(),
            os: "linux".to_string(),
            arch: "x86_64".to_string(),
            session_total: 7,
            session_running: 1,
            session_idle: 6,
            session_error: 0,
            session_cockpit: 0,
            session_sandboxed: 2,
            session_yolo: 0,
            session_pinned: 0,
            session_snoozed: 0,
            session_archived: 0,
            sessions_by_agent: BTreeMap::new(),
            sessions_by_model_bucket: BTreeMap::new(),
            sessions_by_substrate: SUBSTRATES.iter().map(|s| (s.to_string(), 0)).collect(),
            features: BTreeMap::new(),
            usage_seen: usage_signals::zeroed(),
            session_creates_since_last_snapshot: 0,
            auth_mode: None,
            serve_mode: None,
        }
    }

    use crate::session::Instance;

    // A maintainer with two pinned sessions and one snoozed session must see
    // `session_pinned = 2` and `session_snoozed = 1` (issue #1892, story 1).
    #[test]
    fn aggregate_counts_each_triage_state() {
        let mut pinned_a = Instance::new("pin-a", "/tmp/a");
        pinned_a.pin();
        let mut pinned_b = Instance::new("pin-b", "/tmp/b");
        pinned_b.pin();
        let mut snoozed = Instance::new("snooze", "/tmp/c");
        snoozed.snooze(60);
        let mut archived = Instance::new("arch", "/tmp/d");
        archived.archive();
        let untouched = Instance::new("plain", "/tmp/e");

        let m = aggregate_instances(&[pinned_a, pinned_b, snoozed, archived, untouched]);

        assert_eq!(m.pinned, 2, "two pinned sessions");
        assert_eq!(m.snoozed, 1, "one currently snoozed session");
        assert_eq!(m.archived, 1, "one archived session");
        assert_eq!(m.total, 5);
    }

    // A snooze whose window has elapsed must not be counted, matching
    // `Instance::is_snoozed()` semantics (issue #1892, story 2).
    #[test]
    fn expired_snooze_is_not_counted() {
        let mut expired = Instance::new("expired", "/tmp/x");
        // A snooze that ended an hour ago: `snoozed_until` is set but in the past.
        expired.snoozed_until = Some(chrono::Utc::now() - chrono::Duration::hours(1));
        assert!(
            !expired.is_snoozed(),
            "precondition: expired snooze reads false"
        );

        let m = aggregate_instances(&[expired]);
        assert_eq!(
            m.snoozed, 0,
            "an elapsed snooze must not increment session_snoozed"
        );
    }

    // The triage census emits only integer counts; the fields are plain `u32`
    // and carry no session id, name, path, or timestamp (issue #1892, story 3).
    #[test]
    fn triage_counts_are_plain_integers() {
        // Assert the wire format, not just the Rust type: a future serde
        // attribute or wrapper that serialized these as strings or null would
        // regress the telemetry contract while a `u32`-only check still passed.
        let json = serde_json::to_value(sample_snapshot()).unwrap();
        assert!(json["session_pinned"].is_u64());
        assert!(json["session_snoozed"].is_u64());
        assert!(json["session_archived"].is_u64());
    }

    // An opted-out install records nothing: `build_usage_snapshot` returns
    // `None` regardless of session state (issue #1892, story 4). `DO_NOT_TRACK`
    // is the absolute, config-independent suppressor.
    #[test]
    #[serial]
    fn opted_out_build_returns_none() {
        unsafe { std::env::set_var("DO_NOT_TRACK", "1") };
        let mut pinned = Instance::new("pin", "/tmp/p");
        pinned.pin();
        assert!(
            build_usage_snapshot(
                Surface::Tui,
                &[pinned],
                usage_signals::zeroed(),
                0,
                None,
                None
            )
            .is_none(),
            "opted-out install must not build a snapshot"
        );
        unsafe { std::env::remove_var("DO_NOT_TRACK") };
    }

    // The serve deployment-mode fields are part of the content fingerprint, so a
    // daemon that switches exposure or auth mode between snapshots is not deduped
    // away as an unchanged repeat (#1885).
    #[test]
    #[serial]
    fn serve_mode_fields_change_the_fingerprint() {
        let base = sample_snapshot();
        let mut serve = sample_snapshot();
        serve.auth_mode = Some("passphrase".to_string());
        serve.serve_mode = Some("tailscale".to_string());
        assert_ne!(
            snapshot_fingerprint(&base),
            snapshot_fingerprint(&serve),
            "adding auth_mode / serve_mode must change the fingerprint"
        );

        let mut other = serve.clone();
        other.serve_mode = Some("tunnel".to_string());
        assert_ne!(
            snapshot_fingerprint(&serve),
            snapshot_fingerprint(&other),
            "a different serve_mode must change the fingerprint"
        );
    }

    // Regression for the duplicate `usage_snapshot` seen in dogfooding: the TUI
    // (and serve) emit a snapshot at boot and another on graceful exit, so a
    // launch-then-quit with unchanged sessions posted the identical payload
    // twice within seconds. The exit path now dedups against the last emit.
    #[test]
    #[serial]
    fn exit_snapshot_dedups_against_boot_but_resends_on_change() {
        *LAST_SNAPSHOT_FP.lock().unwrap() = None;

        // A confirmed boot send records the fingerprint (this is what
        // `send_snapshot` does on success).
        let boot = sample_snapshot();
        record_snapshot_fp(&boot);

        // Quit right after, sessions unchanged: same content, newer stamp.
        // The only difference is `sent_at`, which the fingerprint excludes, so
        // the exit snapshot is recognised as a duplicate and not re-sent.
        let mut exit = sample_snapshot();
        exit.sent_at = "2026-06-02T19:00:47Z".to_string();
        assert!(
            snapshot_matches_last(&exit),
            "an unchanged exit snapshot must dedupe against the boot snapshot"
        );

        // A snapshot whose counts actually changed is not a duplicate, so it
        // would be sent; a confirmed send then makes it the new baseline.
        let mut changed = sample_snapshot();
        changed.session_total = 8;
        assert!(
            !snapshot_matches_last(&changed),
            "a changed snapshot must still be emitted"
        );
        record_snapshot_fp(&changed);
        let mut changed_again = changed.clone();
        changed_again.sent_at = "2026-06-02T19:05:00Z".to_string();
        assert!(
            snapshot_matches_last(&changed_again),
            "repeating the latest snapshot dedups against it"
        );

        *LAST_SNAPSHOT_FP.lock().unwrap() = None;
    }

    // The fingerprint is recorded only by `send_snapshot` on a confirmed send,
    // never by `snapshot_matches_last` (a pure peek). So checking a snapshot
    // without a confirmed send must not poison the dedup cache: a failed boot
    // send leaves the next identical snapshot eligible to retry, instead of
    // being silently dropped as a "duplicate" of something never delivered.
    #[test]
    #[serial]
    fn peek_does_not_record_fingerprint() {
        *LAST_SNAPSHOT_FP.lock().unwrap() = None;
        let snap = sample_snapshot();
        assert!(
            !snapshot_matches_last(&snap),
            "first peek must not match an empty cache"
        );
        assert!(
            !snapshot_matches_last(&snap),
            "peeking must not record the fingerprint, so it still does not match"
        );
        *LAST_SNAPSHOT_FP.lock().unwrap() = None;
    }

    // Item B (#1877): the pure assembler builds a snapshot from an injected
    // `Config` and install id with no disk reads, no opt-in gate, and no id
    // generation. `build_usage_snapshot` therefore parses `config.toml` exactly
    // once (for both the opt-in check and `active_features`) instead of twice.
    #[test]
    fn assemble_usage_snapshot_uses_injected_config_without_disk() {
        use crate::session::{Config, Instance};
        let config = Config::default();
        let inst = Instance::new("s", "/p");

        let snapshot = assemble_usage_snapshot(
            Surface::Tui,
            "test-install-id".to_string(),
            &config,
            std::slice::from_ref(&inst),
            usage_signals::zeroed(),
            3,
        );

        assert_eq!(snapshot.install_id, "test-install-id");
        assert_eq!(snapshot.session_total, 1);
        assert_eq!(snapshot.session_creates_since_last_snapshot, 3);
        // The feature map comes from the injected config, proving the assembler
        // consumes it rather than re-loading from disk.
        assert_eq!(snapshot.features, features::active_features(&config));
    }

    // Item D (#1877): the jittered snapshot period always lands in
    // `[base, base + jitter)`, so the cadence is bounded while still spreading
    // installs apart.
    #[test]
    fn snapshot_interval_stays_within_jitter_bound() {
        for _ in 0..1000 {
            let period = snapshot_interval();
            assert!(period >= SNAPSHOT_BASE_INTERVAL, "below base: {period:?}");
            assert!(
                period < SNAPSHOT_BASE_INTERVAL + SNAPSHOT_JITTER,
                "above base+jitter: {period:?}"
            );
        }
    }
}
