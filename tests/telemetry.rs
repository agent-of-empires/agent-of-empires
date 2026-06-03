//! Integration tests for the opt-in telemetry user stories (issue #1762).
//!
//! These mutate process-global env (`HOME` / `XDG_CONFIG_HOME` to redirect the
//! app dir, plus `DO_NOT_TRACK` / `AOE_TELEMETRY_ENDPOINT`), so every test is
//! `#[serial]`. Each test points the app dir at a fresh `TempDir`, so no real
//! user state is touched.

use agent_of_empires::session::{save_config, Config, Instance};
use agent_of_empires::telemetry::{self, Surface};
use agent_of_empires::update::{ReleasesBehind, UpdateStatus};
use serial_test::serial;

/// Redirect the app dir at a temp location and clear the telemetry-related env
/// vars. Returns the guard; keep it alive for the test's duration.
fn isolate() -> tempfile::TempDir {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    unsafe {
        std::env::set_var("HOME", tmp.path());
        std::env::set_var("XDG_CONFIG_HOME", tmp.path());
        std::env::remove_var("DO_NOT_TRACK");
        std::env::remove_var("AOE_TELEMETRY_ENDPOINT");
    }
    tmp
}

fn set_enabled(enabled: bool) {
    let mut config = Config::load_or_warn();
    config.telemetry.enabled = enabled;
    save_config(&config).expect("save config");
}

/// Write a synthetic update-check cache into the isolated app dir so the
/// version-health classifiers have deterministic input (no network). `releases`
/// is newest-first, matching what the updater stores.
fn write_update_cache(latest: &str, releases: &[&str]) {
    let dir = agent_of_empires::session::get_app_dir().expect("app dir");
    let releases_json: Vec<_> = releases
        .iter()
        .map(|v| serde_json::json!({ "version": v, "body": "", "published_at": null }))
        .collect();
    let cache = serde_json::json!({
        "checked_at": "2026-06-03T00:00:00Z",
        "latest_version": latest,
        "releases": releases_json,
    });
    std::fs::write(
        dir.join("update_cache.json"),
        serde_json::to_string(&cache).expect("serialize cache"),
    )
    .expect("write update cache");
}

/// Default-off must hold: a fresh install reports no opt-in, no install id,
/// and builds no events.
#[test]
#[serial]
fn default_off_emits_nothing() {
    let _tmp = isolate();
    assert!(!telemetry::is_opted_in());
    assert_eq!(telemetry::install_id(), None);
    assert!(telemetry::build_process_start(Surface::Cli).is_none());
    assert!(telemetry::build_usage_snapshot(Surface::Tui, &[], false, false, 0).is_none());
}

/// Opting in generates an install id and lets events build; opting back out
/// deletes the id.
#[test]
#[serial]
fn opt_in_round_trips_and_opt_out_deletes_id() {
    let _tmp = isolate();

    set_enabled(true);
    telemetry::apply_opt_in_change(true);
    assert!(telemetry::is_opted_in());
    let id = telemetry::install_id().expect("id generated on opt-in");
    assert!(!id.is_empty());

    let event = telemetry::build_process_start(Surface::Tui).expect("event built when opted in");
    assert_eq!(event.surface, Surface::Tui);
    assert_eq!(event.event, "process_start");
    assert_eq!(event.install_id, id);

    // Opt back out: id deleted, events stop building.
    set_enabled(false);
    telemetry::apply_opt_in_change(false);
    assert!(!telemetry::is_opted_in());
    assert_eq!(telemetry::install_id(), None);
    assert!(telemetry::build_process_start(Surface::Tui).is_none());
}

/// `DO_NOT_TRACK` is absolute: even with the config flag on, nothing is opted
/// in, no install id is generated, and no events build.
#[test]
#[serial]
fn do_not_track_suppresses_send_and_id() {
    let _tmp = isolate();
    set_enabled(true);
    unsafe { std::env::set_var("DO_NOT_TRACK", "1") };

    assert!(telemetry::do_not_track());
    assert!(!telemetry::is_opted_in());
    // apply_opt_in_change must NOT generate an id while suppressed.
    telemetry::apply_opt_in_change(true);
    assert_eq!(telemetry::install_id(), None);
    assert!(telemetry::build_process_start(Surface::Cli).is_none());

    unsafe { std::env::remove_var("DO_NOT_TRACK") };
}

/// The snapshot payload carries only allowlisted buckets: a custom agent
/// command and a custom model collapse to `custom` / `other`, never the raw
/// strings.
#[test]
#[serial]
fn snapshot_buckets_are_sanitized() {
    let _tmp = isolate();
    set_enabled(true);
    telemetry::apply_opt_in_change(true);

    let mut custom = Instance::new("secret-session", "/home/me/secret-project");
    custom.tool = "/usr/local/bin/my-internal-agent".to_string();
    custom.detect_as = String::new();
    let claude = Instance::new("c", "/p");

    let snapshot =
        telemetry::build_usage_snapshot(Surface::Tui, &[custom, claude], false, false, 0)
            .expect("snapshot built when opted in");

    let serialized = serde_json::to_string(&snapshot).expect("serialize");
    // The raw custom command / project path must never appear in the payload.
    assert!(!serialized.contains("my-internal-agent"));
    assert!(!serialized.contains("secret-project"));
    assert!(!serialized.contains("secret-session"));

    assert_eq!(snapshot.sessions_by_agent.get("custom"), Some(&1));
    assert_eq!(snapshot.sessions_by_agent.get("claude"), Some(&1));
    assert_eq!(snapshot.session_total, 2);

    // The feature-adoption map is present with its fixed allowlisted keys
    // (values reflect config; all false under a default config).
    for key in ["worktree", "sandbox", "cockpit", "auto_update"] {
        assert!(
            snapshot.features.contains_key(key),
            "features map missing allowlisted key `{key}`"
        );
    }
}

/// User story (#1874): the create-trend counter carries a real value. When N
/// sessions were created during the window, the snapshot reports
/// `session_creates_since_last_snapshot == N`; with none created it reports 0.
#[test]
#[serial]
fn snapshot_carries_session_create_count() {
    let _tmp = isolate();
    set_enabled(true);
    telemetry::apply_opt_in_change(true);

    let none = telemetry::build_usage_snapshot(Surface::Serve, &[], false, false, 0)
        .expect("snapshot built when opted in");
    assert_eq!(none.session_creates_since_last_snapshot, 0);

    let some = telemetry::build_usage_snapshot(Surface::Serve, &[], false, false, 7)
        .expect("snapshot built when opted in");
    assert_eq!(some.session_creates_since_last_snapshot, 7);
}

/// The CLI `process_start` is throttled to once per install per day so a user
/// scripting `aoe` in a loop can't flood the endpoint: a send is due first, then
/// not due once a confirmed send claims the daily slot.
#[test]
#[serial]
fn cli_process_start_throttled_to_once_per_window() {
    let _tmp = isolate();
    set_enabled(true);
    telemetry::apply_opt_in_change(true);

    let day = std::time::Duration::from_secs(24 * 60 * 60);
    let hour = std::time::Duration::from_secs(60 * 60);
    assert!(
        telemetry::cli_process_start_due(day, hour),
        "first send in the window should be due"
    );
    // A confirmed send claims the daily slot.
    telemetry::record_cli_process_start(true);
    assert!(
        !telemetry::cli_process_start_due(day, hour),
        "within the day, no further send is due after a confirmed send"
    );
    // Zero gaps always re-grant (every stamp is always older than zero).
    assert!(telemetry::cli_process_start_due(
        std::time::Duration::ZERO,
        std::time::Duration::ZERO
    ));
}

/// User story (#1875): when a CLI `process_start` send fails, the daily throttle
/// slot is NOT consumed, so the next invocation retries instead of losing the
/// whole day to one transient failure. The retry gap still bounds how often the
/// failed send is re-attempted.
#[test]
#[serial]
fn failed_cli_process_start_leaves_daily_slot_open() {
    let _tmp = isolate();
    set_enabled(true);
    telemetry::apply_opt_in_change(true);

    let day = std::time::Duration::from_secs(24 * 60 * 60);
    let hour = std::time::Duration::from_secs(60 * 60);

    // Simulate a failed send: it stamps the attempt but never claims the slot.
    telemetry::record_cli_process_start(false);

    // The retry gap blocks an immediate re-attempt against a still-down endpoint.
    assert!(
        !telemetry::cli_process_start_due(day, hour),
        "retry gap must block an immediate re-attempt after a failed send"
    );
    // But the daily slot is still open: once the retry gap elapses, a send is due
    // again, unlike the old behaviour that lost the whole day on one failure.
    assert!(
        telemetry::cli_process_start_due(day, std::time::Duration::ZERO),
        "a failed send must leave the daily slot open for retry"
    );
}

/// An unreachable / slow endpoint must never block the CLI: `flush_process_start`
/// is bounded and returns well within the timeout even when the endpoint
/// black-holes the connection.
#[test]
#[serial]
fn unreachable_endpoint_never_blocks() {
    let _tmp = isolate();
    set_enabled(true);
    telemetry::apply_opt_in_change(true);
    // 127.0.0.1:9 (discard) with nothing listening: connection refused fast,
    // but the bound is what guarantees we never hang regardless.
    unsafe { std::env::set_var("AOE_TELEMETRY_ENDPOINT", "http://127.0.0.1:9/ingest") };

    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let start = std::time::Instant::now();
    rt.block_on(telemetry::flush_process_start(Surface::Cli));
    let elapsed = start.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "flush_process_start blocked for {elapsed:?}; must be bounded"
    );

    unsafe { std::env::remove_var("AOE_TELEMETRY_ENDPOINT") };
}

/// User story (#1887): an opted-in install reports its data-schema version on
/// `process_start` (covers all surfaces, including CLI-only installs). It is the
/// build's `migrations::CURRENT_VERSION`, a small integer.
#[test]
#[serial]
fn process_start_carries_data_schema_version() {
    let _tmp = isolate();
    set_enabled(true);
    telemetry::apply_opt_in_change(true);

    let event = telemetry::build_process_start(Surface::Cli).expect("event built when opted in");
    assert_eq!(
        event.data_schema_version,
        agent_of_empires::migrations::current_schema_version()
    );
    // With no update cache yet, staleness is Unknown, never a false "current".
    assert_eq!(event.update_status, UpdateStatus::Unknown);
    assert_eq!(event.update_releases_behind, ReleasesBehind::Unknown);
}

/// User story (#1887): when the cached update check shows a newer release, both
/// events reflect the staleness buckets. A latest far above any real build is
/// `major_behind`; two cached newer releases is `several_behind`, one is
/// `one_behind`.
#[test]
#[serial]
fn version_health_reflects_cached_update() {
    let _tmp = isolate();
    set_enabled(true);
    telemetry::apply_opt_in_change(true);

    // Two cached releases newer than any plausible current build.
    write_update_cache("9999.0.0", &["9999.0.0", "9998.0.0"]);
    let snapshot = telemetry::build_usage_snapshot(Surface::Serve, &[], false, false, 0)
        .expect("snapshot built when opted in");
    assert_eq!(snapshot.update_status, UpdateStatus::MajorBehind);
    assert_eq!(
        snapshot.update_releases_behind,
        ReleasesBehind::SeveralBehind
    );

    let event = telemetry::build_process_start(Surface::Cli).expect("event built when opted in");
    assert_eq!(event.update_status, UpdateStatus::MajorBehind);
    assert_eq!(event.update_releases_behind, ReleasesBehind::SeveralBehind);

    // Exactly one cached newer release is one_behind.
    write_update_cache("9999.0.0", &["9999.0.0"]);
    let snapshot = telemetry::build_usage_snapshot(Surface::Serve, &[], false, false, 0)
        .expect("snapshot built when opted in");
    assert_eq!(snapshot.update_releases_behind, ReleasesBehind::OneBehind);
}

/// User story (#1887, privacy reviewer): version-health signals leave the client
/// only as a small integer and coarse bucket strings. The raw cached "latest"
/// version string is never serialized into either event.
#[test]
#[serial]
fn version_health_never_leaks_version_string() {
    let _tmp = isolate();
    set_enabled(true);
    telemetry::apply_opt_in_change(true);

    let secret_latest = "9999.1234.5678";
    write_update_cache(secret_latest, &[secret_latest]);

    let snapshot = telemetry::build_usage_snapshot(Surface::Serve, &[], false, false, 0)
        .expect("snapshot built when opted in");
    let event = telemetry::build_process_start(Surface::Tui).expect("event built when opted in");

    for serialized in [
        serde_json::to_string(&snapshot).expect("serialize snapshot"),
        serde_json::to_string(&event).expect("serialize process_start"),
    ] {
        assert!(
            !serialized.contains(secret_latest),
            "raw cached latest version leaked into payload: {serialized}"
        );
        assert!(
            !serialized.contains("1234"),
            "a fragment of the cached version leaked: {serialized}"
        );
        // Only the coarse bucket leaves the client.
        assert!(
            serialized.contains("major_behind"),
            "expected the coarse staleness bucket in the payload: {serialized}"
        );
    }
}

/// User story (#1887, opted-out): even with an update cache present and a schema
/// version on disk, nothing is built or sent when the user has not opted in.
#[test]
#[serial]
fn opted_out_emits_nothing_even_with_version_health_available() {
    let _tmp = isolate();
    // Cache present, but telemetry left at its default-off state.
    write_update_cache("9999.0.0", &["9999.0.0", "9998.0.0"]);

    assert!(!telemetry::is_opted_in());
    assert!(telemetry::build_process_start(Surface::Cli).is_none());
    assert!(telemetry::build_usage_snapshot(Surface::Serve, &[], false, false, 0).is_none());
}
