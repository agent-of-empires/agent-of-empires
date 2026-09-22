//! Anonymous, opt-in usage telemetry. Off by default, `DO_NOT_TRACK` always wins, sends are
//! fire-and-forget with hard timeouts, and every field is sanitized to closed allowlists.

pub mod aggregate;
pub mod events;
pub mod features;
pub mod form_factor;
pub mod plugins;
pub mod sanitize;
mod state;
pub mod usage_signals;

use std::collections::BTreeMap;
use std::time::Duration;

pub use events::{
    CliUsage, ProcessStart, StructuredInteractionCounts, Surface, UsageSnapshot, SCHEMA_VERSION,
};
pub use form_factor::WebClientFormFactor;
pub use state::{
    cli_usage_due, ensure_install_id, install_id, record_cli_command, record_cli_usage_flush,
    reset_install_id,
};

use crate::session::Instance;

const SEND_TIMEOUT: Duration = Duration::from_secs(2);

const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);

const DEFAULT_ENDPOINT: &str = "https://telemetry.agent-of-empires.com/v1/ingest";

/// Not authentication: it only lets the gateway drop unkeyed drive-by traffic.
const TELEMETRY_KEY: &str = "7bc5a4e45ce861662b9690a7105da988";

/// `cli_usage` is throttled to once per install per day; counts accumulate between flushes.
const CLI_USAGE_MIN_GAP: Duration = Duration::from_secs(24 * 60 * 60);

/// Bounds retries after a failed send while the daily slot stays open.
const CLI_USAGE_RETRY_GAP: Duration = Duration::from_secs(60 * 60);

pub const SNAPSHOT_BASE_INTERVAL: Duration = Duration::from_secs(4 * 60 * 60);

const SNAPSHOT_JITTER: Duration = Duration::from_secs(30 * 60);

/// Per-process jitter keeps a fleet that boots together from snapshotting in lockstep.
pub fn snapshot_interval() -> Duration {
    use rand::RngExt;
    let jitter_ms = rand::rng().random_range(0..SNAPSHOT_JITTER.as_millis() as u64);
    SNAPSHOT_BASE_INTERVAL + Duration::from_millis(jitter_ms)
}

pub fn do_not_track() -> bool {
    match std::env::var("DO_NOT_TRACK") {
        Ok(v) => {
            let v = v.trim().to_ascii_lowercase();
            matches!(v.as_str(), "1" | "true" | "yes")
        }
        Err(_) => false,
    }
}

pub fn endpoint() -> String {
    match std::env::var("AOE_TELEMETRY_ENDPOINT") {
        Ok(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => DEFAULT_ENDPOINT.to_string(),
    }
}

pub fn is_opted_in() -> bool {
    crate::session::get_telemetry_settings().enabled && !do_not_track()
}

fn opted_in_with(config: &crate::session::Config) -> bool {
    config.telemetry.enabled && !do_not_track()
}

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

const SUBSTRATES: [&str; 5] = ["scratch", "workspace", "worktree", "sandbox", "local"];

/// Precedence: scratch > workspace > worktree > sandbox > local.
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

pub fn build_process_start(surface: Surface) -> Option<ProcessStart> {
    if !is_opted_in() {
        return None;
    }
    let install_id = state::ensure_install_id()?;
    let (update_status, update_releases_behind) =
        crate::update::cached_version_health(env!("CARGO_PKG_VERSION"));
    Some(ProcessStart {
        schema: SCHEMA_VERSION,
        event: "process_start",
        uuid: uuid::Uuid::new_v4().to_string(),
        install_id,
        sent_at: now_rfc3339(),
        surface,
        aoe_version: env!("CARGO_PKG_VERSION").to_string(),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        data_schema_version: crate::migrations::current_schema_version(),
        update_status,
        update_releases_behind,
    })
}

struct InstanceMetrics {
    total: u32,
    trashed: u32,
    running: u32,
    idle: u32,
    error: u32,
    acp: u32,
    sandboxed: u32,
    yolo: u32,
    pinned: u32,
    snoozed: u32,
    archived: u32,
    by_agent: BTreeMap<String, u32>,
    by_model_bucket: BTreeMap<String, u32>,
    by_substrate: BTreeMap<String, u32>,
}

pub(crate) fn instance_buckets(inst: &Instance) -> (String, String) {
    let agent_src = if inst.detect_as.trim().is_empty() {
        inst.tool.as_str()
    } else {
        inst.detect_as.as_str()
    };
    let model = inst.agent_model.as_deref();
    (
        sanitize::agent_bucket(agent_src),
        sanitize::model_bucket(model).to_string(),
    )
}

fn aggregate_instances(instances: &[Instance]) -> InstanceMetrics {
    let mut by_agent: BTreeMap<String, u32> = BTreeMap::new();
    let mut by_model_bucket: BTreeMap<String, u32> = BTreeMap::new();
    let mut by_substrate: BTreeMap<String, u32> =
        SUBSTRATES.iter().map(|s| (s.to_string(), 0)).collect();
    let (mut running, mut idle, mut error, mut acp, mut sandboxed, mut yolo) =
        (0u32, 0u32, 0u32, 0u32, 0u32, 0u32);
    let (mut pinned, mut snoozed, mut archived) = (0u32, 0u32, 0u32);
    let (mut total, mut trashed) = (0u32, 0u32);

    for inst in instances {
        // Trash is a pending delete, reported only as its own count.
        if inst.is_trashed() {
            trashed += 1;
            continue;
        }
        total += 1;

        match inst.status {
            crate::session::Status::Running => running += 1,
            crate::session::Status::Idle => idle += 1,
            crate::session::Status::Error => error += 1,
            _ => {}
        }
        let is_structured = inst.is_structured();
        if is_structured {
            acp += 1;
        }
        if inst.sandbox_info.as_ref().is_some_and(|s| s.enabled) {
            sandboxed += 1;
        }
        if inst.yolo_mode {
            yolo += 1;
        }

        // Increment the pre-seeded key so drift in `substrate_bucket` fails loudly.
        *by_substrate
            .get_mut(substrate_bucket(inst))
            .expect("SUBSTRATES must contain every substrate bucket") += 1;

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

        let (agent, model) = instance_buckets(inst);
        *by_agent.entry(agent).or_insert(0) += 1;
        *by_model_bucket.entry(model).or_insert(0) += 1;
    }

    InstanceMetrics {
        total,
        trashed,
        running,
        idle,
        error,
        acp,
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

pub fn build_usage_snapshot(
    surface: Surface,
    instances: &[Instance],
    usage_seen: BTreeMap<String, u32>,
    session_creates_since_last_snapshot: u32,
    auth_mode: Option<&str>,
    serve_mode: Option<&str>,
    acp_counts: &StructuredInteractionCounts,
) -> Option<UsageSnapshot> {
    let config = crate::session::Config::load_or_warn();
    if !opted_in_with(&config) {
        return None;
    }
    // auth_mode and serve_mode are daemon-only metadata.
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
        acp_counts,
    );
    snapshot.auth_mode = auth_mode.map(str::to_string);
    snapshot.serve_mode = serve_mode.map(str::to_string);
    let (update_status, update_releases_behind) =
        crate::update::cached_version_health(env!("CARGO_PKG_VERSION"));
    snapshot.data_schema_version = crate::migrations::current_schema_version();
    snapshot.update_status = update_status;
    snapshot.update_releases_behind = update_releases_behind;
    let (plugins_by_source, plugins_active) = plugins::census(crate::plugin::registry().all());
    snapshot.plugins_by_source = plugins_by_source;
    snapshot.plugins_active = plugins_active;
    Some(snapshot)
}

fn assemble_usage_snapshot(
    surface: Surface,
    install_id: String,
    config: &crate::session::Config,
    instances: &[Instance],
    usage_seen: BTreeMap<String, u32>,
    session_creates_since_last_snapshot: u32,
    acp_counts: &StructuredInteractionCounts,
) -> UsageSnapshot {
    let features = features::active_features(config);

    let metrics = aggregate_instances(instances);

    UsageSnapshot {
        schema: SCHEMA_VERSION,
        event: "usage_snapshot",
        uuid: uuid::Uuid::new_v4().to_string(),
        install_id,
        sent_at: now_rfc3339(),
        surface,
        aoe_version: env!("CARGO_PKG_VERSION").to_string(),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        data_schema_version: 0,
        update_status: crate::update::UpdateStatus::Unknown,
        update_releases_behind: crate::update::ReleasesBehind::Unknown,
        session_total: metrics.total,
        session_running: metrics.running,
        session_idle: metrics.idle,
        session_error: metrics.error,
        session_structured: metrics.acp,
        session_sandboxed: metrics.sandboxed,
        session_yolo: metrics.yolo,
        peak_concurrent_sessions: metrics.total,
        session_pinned: metrics.pinned,
        session_snoozed: metrics.snoozed,
        session_archived: metrics.archived,
        session_trashed: metrics.trashed,
        sessions_by_agent: metrics.by_agent,
        sessions_by_model_bucket: metrics.by_model_bucket,
        sessions_by_substrate: metrics.by_substrate,
        distinct_sessions_by_agent: BTreeMap::new(),
        distinct_sessions_by_model_bucket: BTreeMap::new(),
        features,
        usage_seen,
        web_clients_seen: BTreeMap::new(),
        structured_clients_seen: BTreeMap::new(),
        session_creates_since_last_snapshot,
        auth_mode: None,
        serve_mode: None,
        approvals_resolved: acp_counts.approvals_resolved(),
        approvals_by_decision: acp_counts.approvals_by_decision(),
        agent_switches: acp_counts.agent_switches,
        plan_mode_seen: acp_counts.plan_mode_seen,
        prompts_queued: acp_counts.prompts_queued,
        plugins_by_source: BTreeMap::new(),
        plugins_active: BTreeMap::new(),
    }
}

/// True only on a 2xx, so callers consume a signal only after confirmed delivery.
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

pub fn spawn_process_start(surface: Surface) {
    if let Some(event) = build_process_start(surface) {
        tokio::spawn(async move {
            post(&event).await;
        });
    }
}

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

/// Records only when opted in; the daily slot is claimed only after a confirmed send.
pub async fn track_cli_command(name: &str) {
    // Opt-in creates the app dir, so its absence means not opted in; never create it here.
    if !crate::session::app_dir_exists() || !is_opted_in() {
        return;
    }
    state::record_cli_command(name);
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
    record_cli_usage_flush(confirmed);
}

static LAST_SNAPSHOT_FP: std::sync::Mutex<Option<u64>> = std::sync::Mutex::new(None);

/// Excludes the per-emit `sent_at` and `uuid`.
fn snapshot_fingerprint(snapshot: &UsageSnapshot) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut probe = snapshot.clone();
    probe.sent_at = String::new();
    probe.uuid = String::new();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    serde_json::to_string(&probe)
        .unwrap_or_default()
        .hash(&mut hasher);
    hasher.finish()
}

fn record_snapshot_fp(snapshot: &UsageSnapshot) {
    if let Ok(mut last) = LAST_SNAPSHOT_FP.lock() {
        *last = Some(snapshot_fingerprint(snapshot));
    }
}

/// Only confirmed sends are recorded, so a failed send never suppresses a retry.
fn snapshot_matches_last(snapshot: &UsageSnapshot) -> bool {
    let fp = snapshot_fingerprint(snapshot);
    match LAST_SNAPSHOT_FP.lock() {
        Ok(last) => *last == Some(fp),
        Err(_) => false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendOutcome {
    Sent,
    Deduped,
    Failed,
}

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

    use crate::session::test_support::EnvGuard;

    #[test]
    #[serial]
    fn do_not_track_recognises_affirmative_values() {
        let _env = EnvGuard::unset(&["DO_NOT_TRACK"]);
        for v in ["1", "true", "TRUE", "yes", "Yes"] {
            let _value = EnvGuard::set(&[("DO_NOT_TRACK", v)]);
            assert!(do_not_track(), "{v} should suppress");
        }
        for v in ["0", "false", "no", ""] {
            let _value = EnvGuard::set(&[("DO_NOT_TRACK", v)]);
            assert!(!do_not_track(), "{v} should not suppress");
        }
        assert!(!do_not_track());
    }

    #[test]
    #[serial]
    fn endpoint_falls_back_to_default_and_env_overrides() {
        let _env = EnvGuard::unset(&["AOE_TELEMETRY_ENDPOINT"]);
        assert_eq!(endpoint(), DEFAULT_ENDPOINT);
        let _blank = EnvGuard::set(&[("AOE_TELEMETRY_ENDPOINT", "   ")]);
        assert_eq!(endpoint(), DEFAULT_ENDPOINT);
        let _override = EnvGuard::set(&[("AOE_TELEMETRY_ENDPOINT", " https://x/y ")]);
        assert_eq!(endpoint(), "https://x/y");
    }

    fn sample_snapshot() -> UsageSnapshot {
        UsageSnapshot {
            schema: SCHEMA_VERSION,
            event: "usage_snapshot",
            uuid: "11111111-1111-4111-8111-111111111111".to_string(),
            install_id: "00000000-0000-0000-0000-000000000000".to_string(),
            sent_at: "2026-06-02T19:00:45Z".to_string(),
            surface: Surface::Tui,
            aoe_version: "0.0.0".to_string(),
            os: "linux".to_string(),
            arch: "x86_64".to_string(),
            data_schema_version: 11,
            update_status: crate::update::UpdateStatus::Current,
            update_releases_behind: crate::update::ReleasesBehind::Current,
            session_total: 7,
            session_running: 1,
            session_idle: 6,
            session_error: 0,
            session_structured: 0,
            session_sandboxed: 2,
            session_yolo: 0,
            peak_concurrent_sessions: 7,
            session_pinned: 0,
            session_trashed: 0,
            session_snoozed: 0,
            session_archived: 0,
            sessions_by_agent: BTreeMap::new(),
            sessions_by_model_bucket: BTreeMap::new(),
            sessions_by_substrate: SUBSTRATES.iter().map(|s| (s.to_string(), 0)).collect(),
            distinct_sessions_by_agent: BTreeMap::new(),
            distinct_sessions_by_model_bucket: BTreeMap::new(),
            features: BTreeMap::new(),
            usage_seen: usage_signals::zeroed(),
            web_clients_seen: BTreeMap::new(),
            structured_clients_seen: BTreeMap::new(),
            session_creates_since_last_snapshot: 0,
            auth_mode: None,
            serve_mode: None,
            approvals_resolved: 0,
            approvals_by_decision: BTreeMap::new(),
            agent_switches: 0,
            plan_mode_seen: false,
            prompts_queued: 0,
            plugins_by_source: BTreeMap::new(),
            plugins_active: BTreeMap::new(),
        }
    }

    use crate::session::Instance;

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
        let mut trashed = Instance::new("trash", "/tmp/f");
        trashed.trash();
        let mut trashed_archived = Instance::new("trash-arch", "/tmp/g");
        trashed_archived.archive();
        trashed_archived.trash();

        let m = aggregate_instances(&[
            pinned_a,
            pinned_b,
            snoozed,
            archived,
            untouched,
            trashed,
            trashed_archived,
        ]);

        assert_eq!(m.pinned, 2, "two pinned sessions");
        assert_eq!(m.snoozed, 1, "one currently snoozed session");
        assert_eq!(
            m.archived, 1,
            "the trashed-then-archived row must not double-count as archived"
        );
        assert_eq!(m.trashed, 2, "both trashed rows counted separately");
        assert_eq!(m.total, 5, "session_total excludes the two trashed rows");
        assert_eq!(
            m.by_substrate.values().sum::<u32>(),
            m.total,
            "sessions_by_substrate must still sum to session_total"
        );
    }

    #[test]
    fn expired_snooze_is_not_counted() {
        let mut expired = Instance::new("expired", "/tmp/x");
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

    #[test]
    fn triage_counts_are_plain_integers() {
        let json = serde_json::to_value(sample_snapshot()).unwrap();
        assert!(json["session_pinned"].is_u64());
        assert!(json["session_snoozed"].is_u64());
        assert!(json["session_archived"].is_u64());
        assert!(json["session_trashed"].is_u64());
    }

    #[test]
    #[serial]
    fn opted_out_build_returns_none() {
        let _env = EnvGuard::set(&[("DO_NOT_TRACK", "1")]);
        let mut pinned = Instance::new("pin", "/tmp/p");
        pinned.pin();
        assert!(
            build_usage_snapshot(
                Surface::Tui,
                &[pinned],
                usage_signals::zeroed(),
                0,
                None,
                None,
                &StructuredInteractionCounts::default()
            )
            .is_none(),
            "opted-out install must not build a snapshot"
        );
    }

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

    #[test]
    #[serial]
    fn exit_snapshot_dedups_against_boot_but_resends_on_change() {
        *LAST_SNAPSHOT_FP.lock().unwrap() = None;

        let boot = sample_snapshot();
        record_snapshot_fp(&boot);

        let mut exit = sample_snapshot();
        exit.sent_at = "2026-06-02T19:00:47Z".to_string();
        exit.uuid = "22222222-2222-4222-8222-222222222222".to_string();
        assert!(
            snapshot_matches_last(&exit),
            "an unchanged exit snapshot must dedupe against the boot snapshot despite a new uuid"
        );

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
            &StructuredInteractionCounts::default(),
        );

        assert_eq!(snapshot.install_id, "test-install-id");
        assert_eq!(snapshot.session_total, 1);
        assert_eq!(snapshot.session_creates_since_last_snapshot, 3);
        assert_eq!(snapshot.features, features::active_features(&config));
    }

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
