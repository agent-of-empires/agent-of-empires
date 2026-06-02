//! Integration tests for the opt-in telemetry user stories (issue #1762).
//!
//! These mutate process-global env (`HOME` / `XDG_CONFIG_HOME` to redirect the
//! app dir, plus `DO_NOT_TRACK` / `AOE_TELEMETRY_ENDPOINT`), so every test is
//! `#[serial]`. Each test points the app dir at a fresh `TempDir`, so no real
//! user state is touched.

use agent_of_empires::session::{save_config, Config, Instance};
use agent_of_empires::telemetry::{self, Surface};
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

/// The CLI `process_start` is throttled to once per install per day so a user
/// scripting `aoe` in a loop can't flood the endpoint: the first claim in a
/// window succeeds, the next is denied.
#[test]
#[serial]
fn cli_process_start_throttled_to_once_per_window() {
    let _tmp = isolate();
    set_enabled(true);
    telemetry::apply_opt_in_change(true);

    let day = std::time::Duration::from_secs(24 * 60 * 60);
    assert!(
        telemetry::claim_cli_process_start_slot(day),
        "first claim in the window should be granted"
    );
    assert!(
        !telemetry::claim_cli_process_start_slot(day),
        "second claim within the window should be denied"
    );
    // A zero-length window always re-grants (the stamp is always older).
    assert!(telemetry::claim_cli_process_start_slot(
        std::time::Duration::ZERO
    ));
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
