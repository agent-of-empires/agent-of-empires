//! Integration tests for the opt-in telemetry user stories (issue #1762).
//!
//! These mutate process-global env (`HOME` / `XDG_CONFIG_HOME` to redirect the
//! app dir, plus `DO_NOT_TRACK` / `AOE_TELEMETRY_ENDPOINT`), so every test is
//! `#[serial]`. Each test points the app dir at a fresh `TempDir`, so no real
//! user state is touched.

use agent_of_empires::session::{
    save_config, Config, Instance, SandboxInfo, WorkspaceInfo, WorktreeInfo,
};
use agent_of_empires::telemetry::usage_signals::{self, UsageSeenCounters, USAGE_SIGNALS};
use agent_of_empires::telemetry::{self, Surface};
use agent_of_empires::update::{ReleasesBehind, UpdateStatus};
use chrono::Utc;
use serial_test::serial;
use std::sync::{Arc, Barrier};
use std::time::Duration;

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
    assert!(telemetry::build_usage_snapshot(
        Surface::Tui,
        &[],
        usage_signals::zeroed(),
        0,
        None,
        None
    )
    .is_none());
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

    let snapshot = telemetry::build_usage_snapshot(
        Surface::Tui,
        &[custom, claude],
        usage_signals::zeroed(),
        0,
        None,
        None,
    )
    .expect("snapshot built when opted in");

    let serialized = serde_json::to_string(&snapshot).expect("serialize");
    // The raw custom command / project path must never appear in the payload.
    assert!(!serialized.contains("my-internal-agent"));
    assert!(!serialized.contains("secret-project"));
    assert!(!serialized.contains("secret-session"));
    // The TUI surface has no serve deployment mode, so the fields are omitted.
    assert!(snapshot.auth_mode.is_none());
    assert!(snapshot.serve_mode.is_none());
    assert!(!serialized.contains("auth_mode"));
    assert!(!serialized.contains("serve_mode"));

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

/// User story (#1883): the snapshot's per-class client form-factor maps are a
/// presence set on the wire. Only seen classes appear (as `true`); a never-seen
/// class is absent (not `false`); and an empty map is omitted entirely rather
/// than serialized as `{}`. The daemon fills these in `build_serve_snapshot`
/// from its client counters; the form-factor classification and allowlist are
/// unit-tested in `telemetry::form_factor`, and the end-to-end seen-ping path is
/// covered by the live Playwright `telemetry-form-factor` spec.
#[test]
#[serial]
fn form_factor_maps_are_a_presence_set_on_the_wire() {
    let _tmp = isolate();
    set_enabled(true);
    telemetry::apply_opt_in_change(true);

    // A fresh serve snapshot has no classified opens, so both maps are empty and
    // must be omitted from the wire (not emitted as `{}`).
    let mut snapshot = telemetry::build_usage_snapshot(
        Surface::Serve,
        &[],
        usage_signals::zeroed(),
        0,
        None,
        None,
    )
    .expect("snapshot built when opted in");
    let empty_wire = serde_json::to_string(&snapshot).expect("serialize");
    assert!(
        !empty_wire.contains("web_clients_seen"),
        "empty web_clients_seen must be skipped, not emitted as {{}}"
    );
    assert!(
        !empty_wire.contains("cockpit_clients_seen"),
        "empty cockpit_clients_seen must be skipped, not emitted as {{}}"
    );

    // With classes recorded (as the daemon would from its client counters), the
    // map carries exactly the seen classes as `true`; never-seen classes stay
    // absent, so it remains a small presence set.
    snapshot
        .web_clients_seen
        .insert("desktop".to_string(), true);
    snapshot
        .web_clients_seen
        .insert("mobile_pwa".to_string(), true);
    assert_eq!(snapshot.web_clients_seen.get("desktop"), Some(&true));
    assert_eq!(snapshot.web_clients_seen.get("mobile_pwa"), Some(&true));
    assert_eq!(snapshot.web_clients_seen.get("mobile"), None);
    let wire = serde_json::to_string(&snapshot).expect("serialize");
    assert!(wire.contains("web_clients_seen"));
    assert!(wire.contains("mobile_pwa"));
}

/// The fixed, closed substrate vocabulary (#1886). The snapshot must never
/// emit a key outside this set.
const SUBSTRATE_VOCAB: [&str; 5] = ["local", "worktree", "workspace", "sandbox", "scratch"];

fn with_worktree(mut inst: Instance) -> Instance {
    inst.worktree_info = Some(WorktreeInfo {
        branch: "feature/x".to_string(),
        main_repo_path: "/repo".to_string(),
        managed_by_aoe: true,
        created_at: Utc::now(),
        base_branch: None,
    });
    inst
}

fn with_workspace(mut inst: Instance) -> Instance {
    inst.workspace_info = Some(WorkspaceInfo {
        branch: "feature/x".to_string(),
        workspace_dir: "/ws".to_string(),
        repos: Vec::new(),
        created_at: Utc::now(),
        cleanup_on_delete: true,
    });
    inst
}

fn with_sandbox(mut inst: Instance, enabled: bool) -> Instance {
    inst.sandbox_info = Some(SandboxInfo {
        enabled,
        container_id: None,
        image: "secret-internal-image:latest".to_string(),
        container_name: "aoe_secret_container".to_string(),
        extra_env: None,
        custom_instruction: None,
    });
    inst
}

/// User story (#1886): a maintainer with one local, one worktree, and one
/// sandboxed session sees one count in each of the matching substrate buckets.
#[test]
#[serial]
fn substrate_census_counts_each_bucket() {
    let _tmp = isolate();
    set_enabled(true);
    telemetry::apply_opt_in_change(true);

    let local = Instance::new("a", "/p");
    let worktree = with_worktree(Instance::new("b", "/p"));
    let sandbox = with_sandbox(Instance::new("c", "/p"), true);

    let snapshot = telemetry::build_usage_snapshot(
        Surface::Tui,
        &[local, worktree, sandbox],
        usage_signals::zeroed(),
        0,
        None,
        None,
    )
    .expect("snapshot built when opted in");

    assert_eq!(snapshot.sessions_by_substrate.get("local"), Some(&1));
    assert_eq!(snapshot.sessions_by_substrate.get("worktree"), Some(&1));
    assert_eq!(snapshot.sessions_by_substrate.get("sandbox"), Some(&1));
    // Untouched buckets are still present (pre-seeded) and zero.
    assert_eq!(snapshot.sessions_by_substrate.get("workspace"), Some(&0));
    assert_eq!(snapshot.sessions_by_substrate.get("scratch"), Some(&0));
}

/// User story (#1886): a session that is both scratch and (somehow) carries
/// worktree info is classified into exactly one bucket by the documented
/// precedence (scratch wins), never double-counted. The substrate buckets
/// always partition `session_total`, so they sum to it.
#[test]
#[serial]
fn substrate_buckets_are_mutually_exclusive_and_sum_to_total() {
    let _tmp = isolate();
    set_enabled(true);
    telemetry::apply_opt_in_change(true);

    // Impossible-but-defensive combo: scratch AND worktree set. Precedence puts
    // it in `scratch`, and it is counted exactly once.
    let mut conflicted = with_worktree(Instance::new("a", "/p"));
    conflicted.scratch = true;
    // A sandboxed worktree buckets as `worktree` (sandbox sits below worktree),
    // yet still increments the orthogonal `session_sandboxed` count.
    let sandboxed_worktree = with_sandbox(with_worktree(Instance::new("b", "/p")), true);
    let workspace = with_workspace(Instance::new("c", "/p"));
    let local = Instance::new("d", "/p");

    let instances = [conflicted, sandboxed_worktree, workspace, local];
    let total = instances.len() as u32;
    let snapshot = telemetry::build_usage_snapshot(
        Surface::Tui,
        &instances,
        usage_signals::zeroed(),
        0,
        None,
        None,
    )
    .expect("snapshot built when opted in");

    let sum: u32 = snapshot.sessions_by_substrate.values().sum();
    assert_eq!(
        sum, total,
        "substrate buckets must partition session_total exactly once each"
    );
    assert_eq!(snapshot.session_total, total);
    assert_eq!(snapshot.sessions_by_substrate.get("scratch"), Some(&1));
    assert_eq!(snapshot.sessions_by_substrate.get("worktree"), Some(&1));
    assert_eq!(snapshot.sessions_by_substrate.get("workspace"), Some(&1));
    assert_eq!(snapshot.sessions_by_substrate.get("local"), Some(&1));
    assert_eq!(snapshot.sessions_by_substrate.get("sandbox"), Some(&0));
    // The substrate map is orthogonal to the sandbox count: the sandboxed
    // worktree is bucketed as worktree but still tallied as sandboxed.
    assert_eq!(snapshot.session_sandboxed, 1);
}

/// Privacy: the substrate map keys are only the allowlisted closed vocabulary,
/// never a path, repo name, branch, or sandbox image string (#1886).
#[test]
#[serial]
fn substrate_keys_are_only_allowlisted_vocab() {
    let _tmp = isolate();
    set_enabled(true);
    telemetry::apply_opt_in_change(true);

    let instances = [
        with_sandbox(
            with_worktree(Instance::new("a", "/home/me/secret-project")),
            true,
        ),
        with_workspace(Instance::new("b", "/home/me/secret-workspace")),
    ];
    let snapshot = telemetry::build_usage_snapshot(
        Surface::Serve,
        &instances,
        usage_signals::zeroed(),
        0,
        None,
        None,
    )
    .expect("snapshot built when opted in");

    for key in snapshot.sessions_by_substrate.keys() {
        assert!(
            SUBSTRATE_VOCAB.contains(&key.as_str()),
            "substrate key `{key}` is outside the closed vocabulary"
        );
    }
    // And the raw image/path strings must not leak into the serialized payload.
    let serialized = serde_json::to_string(&snapshot).expect("serialize");
    assert!(!serialized.contains("secret-project"));
    assert!(!serialized.contains("secret-workspace"));
    assert!(!serialized.contains("secret-internal-image"));
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

    let none = telemetry::build_usage_snapshot(
        Surface::Serve,
        &[],
        usage_signals::zeroed(),
        0,
        None,
        None,
    )
    .expect("snapshot built when opted in");
    assert_eq!(none.session_creates_since_last_snapshot, 0);

    let some = telemetry::build_usage_snapshot(
        Surface::Serve,
        &[],
        usage_signals::zeroed(),
        7,
        None,
        None,
    )
    .expect("snapshot built when opted in");
    assert_eq!(some.session_creates_since_last_snapshot, 7);
}

/// User stories (#1873): every built event carries a non-empty per-event
/// `uuid`, distinct from `install_id` and `sent_at`, and two events built in the
/// same process get different uuids. This is the idempotency key the gateway
/// forwards as the PostHog event `uuid` for native redelivery dedup.
#[test]
#[serial]
fn events_carry_distinct_idempotency_uuid() {
    let _tmp = isolate();
    set_enabled(true);
    telemetry::apply_opt_in_change(true);

    let snap =
        telemetry::build_usage_snapshot(Surface::Tui, &[], usage_signals::zeroed(), 0, None, None)
            .expect("snapshot built when opted in");
    assert!(!snap.uuid.is_empty(), "snapshot uuid must be non-empty");
    assert_ne!(
        snap.uuid, snap.install_id,
        "uuid must differ from install_id"
    );
    assert_ne!(snap.uuid, snap.sent_at, "uuid must differ from sent_at");
    // It must serialize onto the wire so the gateway can read it.
    let serialized = serde_json::to_string(&snap).expect("serialize");
    assert!(
        serialized.contains(&format!("\"uuid\":\"{}\"", snap.uuid)),
        "uuid must be present in the serialized payload"
    );

    // Two events built in the same process must not collide.
    let proc = telemetry::build_process_start(Surface::Tui).expect("process_start built");
    let snap2 =
        telemetry::build_usage_snapshot(Surface::Tui, &[], usage_signals::zeroed(), 0, None, None)
            .expect("second snapshot built");
    assert_ne!(
        snap.uuid, snap2.uuid,
        "two snapshots must get distinct uuids"
    );
    assert_ne!(
        proc.uuid, snap.uuid,
        "process_start and snapshot must get distinct uuids"
    );
}

/// Opted out (the default): no event, and therefore no `uuid`, is ever built.
#[test]
#[serial]
fn opted_out_builds_no_uuid() {
    let _tmp = isolate();
    assert!(!telemetry::is_opted_in());
    assert!(telemetry::build_process_start(Surface::Tui).is_none());
    assert!(telemetry::build_usage_snapshot(
        Surface::Tui,
        &[],
        usage_signals::zeroed(),
        0,
        None,
        None
    )
    .is_none());
}

/// User story (#1880): a usage signal registered in the allowlist flows through
/// the daemon aggregate (`UsageSeenCounters`) into the snapshot's `usage_seen`
/// map with no other code changes. The map carries the recorded counts verbatim.
#[test]
#[serial]
fn snapshot_carries_registered_usage_signals() {
    let _tmp = isolate();
    set_enabled(true);
    telemetry::apply_opt_in_change(true);

    // The daemon folds browser pings into these counters.
    let counters = UsageSeenCounters::new();
    assert!(counters.record("web"));
    assert!(counters.record("web"));
    assert!(counters.record("cockpit"));

    let snapshot =
        telemetry::build_usage_snapshot(Surface::Serve, &[], counters.snapshot(), 0, None, None)
            .expect("snapshot built when opted in");
    assert_eq!(snapshot.usage_seen.get("web"), Some(&2));
    assert_eq!(snapshot.usage_seen.get("cockpit"), Some(&1));
}

/// User story (#1880): an unregistered signal name is rejected by the registry
/// (`record` returns false, which the endpoint turns into a 400) and never
/// reaches the snapshot's `usage_seen` map.
#[test]
#[serial]
fn unregistered_usage_signal_is_rejected_and_never_reported() {
    let _tmp = isolate();
    set_enabled(true);
    telemetry::apply_opt_in_change(true);

    let counters = UsageSeenCounters::new();
    // The endpoint would return 400 on this false.
    assert!(!counters.record("web_terminal"));

    let snapshot =
        telemetry::build_usage_snapshot(Surface::Serve, &[], counters.snapshot(), 0, None, None)
            .expect("snapshot built when opted in");
    assert!(!snapshot.usage_seen.contains_key("web_terminal"));
}

/// User story (#1880): the `usage_seen` map only ever carries allowlisted short
/// names, never free-form input. Its key set is exactly the fixed registry and
/// every key is a short identifier.
#[test]
#[serial]
fn usage_seen_keys_are_only_allowlisted_short_names() {
    let _tmp = isolate();
    set_enabled(true);
    telemetry::apply_opt_in_change(true);

    let snapshot = telemetry::build_usage_snapshot(
        Surface::Serve,
        &[],
        usage_signals::zeroed(),
        0,
        None,
        None,
    )
    .expect("snapshot built when opted in");

    // `usage_seen` is a BTreeMap, so its keys come out sorted; compare against
    // the registry sorted the same way rather than relying on its source order.
    let keys: Vec<&str> = snapshot.usage_seen.keys().map(String::as_str).collect();
    let mut expected: Vec<&str> = USAGE_SIGNALS.to_vec();
    expected.sort_unstable();
    assert_eq!(keys, expected);
    for key in snapshot.usage_seen.keys() {
        assert!(
            key.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
            "usage_seen key `{key}` is not a short allowlisted identifier"
        );
    }
}

/// User stories (#1885): the serve snapshot carries the coarse deployment mode.
/// A passphrase-auth daemon behind a Tailscale Funnel reports
/// `auth_mode = "passphrase"` and `serve_mode = "tailscale"`; the token-gated
/// local-only default reports `auth_mode = "token"` and `serve_mode = "local"`.
/// Both fields are always from the closed allowlist and never carry a tunnel
/// name, hostname, token, or passphrase.
#[test]
#[serial]
fn serve_snapshot_carries_coarse_deployment_mode() {
    let _tmp = isolate();
    set_enabled(true);
    telemetry::apply_opt_in_change(true);

    let tailscale = telemetry::build_usage_snapshot(
        Surface::Serve,
        &[],
        usage_signals::zeroed(),
        0,
        Some("passphrase"),
        Some("tailscale"),
    )
    .expect("snapshot built when opted in");
    assert_eq!(tailscale.auth_mode.as_deref(), Some("passphrase"));
    assert_eq!(tailscale.serve_mode.as_deref(), Some("tailscale"));

    let local = telemetry::build_usage_snapshot(
        Surface::Serve,
        &[],
        usage_signals::zeroed(),
        0,
        Some("token"),
        Some("local"),
    )
    .expect("snapshot built when opted in");
    assert_eq!(local.auth_mode.as_deref(), Some("token"));
    assert_eq!(local.serve_mode.as_deref(), Some("local"));

    // Both fields are constrained to their closed sets on the wire.
    let serialized = serde_json::to_string(&tailscale).expect("serialize");
    assert!(serialized.contains("\"auth_mode\":\"passphrase\""));
    assert!(serialized.contains("\"serve_mode\":\"tailscale\""));
}

/// As an opted-out user, serve in any auth/exposure mode records nothing: the
/// snapshot is not even built, regardless of the deployment-mode arguments.
#[test]
#[serial]
fn opted_out_serve_builds_no_snapshot_with_deployment_mode() {
    let _tmp = isolate();
    assert!(!telemetry::is_opted_in());
    assert!(telemetry::build_usage_snapshot(
        Surface::Serve,
        &[],
        usage_signals::zeroed(),
        0,
        Some("none"),
        Some("tunnel"),
    )
    .is_none());
}

/// The CLI `cli_usage` flush is throttled to once per install per day so a user
/// scripting `aoe` in a loop can't flood the endpoint: a send is due first, then
/// not due once a confirmed send claims the daily slot.
#[test]
#[serial]
fn cli_usage_flush_throttled_to_once_per_window() {
    let _tmp = isolate();
    set_enabled(true);
    telemetry::apply_opt_in_change(true);

    let day = std::time::Duration::from_secs(24 * 60 * 60);
    let hour = std::time::Duration::from_secs(60 * 60);
    assert!(
        telemetry::cli_usage_due(day, hour),
        "first send in the window should be due"
    );
    // A confirmed send claims the daily slot.
    telemetry::record_cli_usage_flush(true);
    assert!(
        !telemetry::cli_usage_due(day, hour),
        "within the day, no further send is due after a confirmed send"
    );
    // Zero gaps always re-grant (every stamp is always older than zero).
    assert!(telemetry::cli_usage_due(
        std::time::Duration::ZERO,
        std::time::Duration::ZERO
    ));
}

/// User story (#1875): when a CLI `cli_usage` send fails, the daily throttle
/// slot is NOT consumed, so the next invocation retries instead of losing the
/// whole day to one transient failure. The retry gap still bounds how often the
/// failed send is re-attempted.
#[test]
#[serial]
fn failed_cli_usage_flush_leaves_daily_slot_open() {
    let _tmp = isolate();
    set_enabled(true);
    telemetry::apply_opt_in_change(true);

    let day = Duration::from_secs(24 * 60 * 60);
    let hour = Duration::from_secs(60 * 60);

    // Simulate a failed send: it stamps the attempt but never claims the slot.
    telemetry::record_cli_usage_flush(false);

    // The retry gap blocks an immediate re-attempt against a still-down endpoint.
    assert!(
        !telemetry::cli_usage_due(day, hour),
        "retry gap must block an immediate re-attempt after a failed send"
    );
    // But the daily slot is still open: once the retry gap elapses, a send is due
    // again, unlike the old behaviour that lost the whole day on one failure.
    assert!(
        telemetry::cli_usage_due(day, std::time::Duration::ZERO),
        "a failed send must leave the daily slot open for retry"
    );
}

/// User story (#1879, maintainer): when an opted-in install runs several CLI
/// subcommands, the `cli_usage` event reflects the full mix (with repeats),
/// not just the first command of the day.
#[test]
#[serial]
fn cli_usage_records_each_subcommand() {
    let _tmp = isolate();
    set_enabled(true);
    telemetry::apply_opt_in_change(true);

    telemetry::record_cli_command("add");
    telemetry::record_cli_command("session");
    telemetry::record_cli_command("add");

    let event = telemetry::build_cli_usage().expect("event built when opted in with counts");
    assert_eq!(event.event, "cli_usage");
    assert_eq!(event.surface, telemetry::Surface::Cli);
    assert_eq!(event.command_counts.get("add"), Some(&2));
    assert_eq!(event.command_counts.get("session"), Some(&1));
    assert_eq!(event.command_counts.len(), 2);
    assert!(!event.window_start.is_empty());

    // A confirmed flush resets the window so the next event starts fresh.
    telemetry::record_cli_usage_flush(true);
    assert!(
        telemetry::build_cli_usage().is_none(),
        "counts must reset after a confirmed flush"
    );
}

/// User story (#1879, privacy): a reported command is an allowlisted command
/// name with no args, paths, or flags. The builder filters any key that is not
/// in the closed clap vocabulary, so a corrupt/hand-edited `telemetry.json`
/// cannot smuggle arbitrary strings onto the wire.
#[test]
#[serial]
fn cli_usage_drops_non_allowlisted_keys() {
    let _tmp = isolate();
    set_enabled(true);
    telemetry::apply_opt_in_change(true);

    telemetry::record_cli_command("add");
    // A key that is not a real clap command (as if injected into the state file).
    telemetry::record_cli_command("/home/me/secret-project");

    let event = telemetry::build_cli_usage().expect("event built");
    assert_eq!(event.command_counts.get("add"), Some(&1));
    for key in event.command_counts.keys() {
        assert!(
            agent_of_empires::cli::CLI_COMMAND_NAMES.contains(&key.as_str()),
            "non-allowlisted key `{key}` leaked into command_counts"
        );
        assert!(
            key.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
            "key `{key}` is not an identifier-safe token"
        );
    }
    let serialized = serde_json::to_string(&event).expect("serialize");
    assert!(!serialized.contains("secret-project"));
}

/// User story (#1879, opted-out): when telemetry is off, recording a CLI command
/// builds nothing and the no-op recorder never materializes the app dir.
#[test]
#[serial]
fn cli_usage_default_off_records_nothing() {
    let _tmp = isolate();

    // The per-command tracker is a true no-op for a not-opted-in install: it must
    // not create the app dir (so app-data-free commands stay pure) and must not
    // record or send anything. Checked first, before any opt-in / config read,
    // since reading the config itself materializes the dir; this isolates the
    // tracker's own (non-creating) behavior, which the `app_dir_exists` gate in
    // `track_cli_command` guarantees by short-circuiting before any config load.
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    rt.block_on(telemetry::track_cli_command("add"));
    assert!(
        !agent_of_empires::session::app_dir_exists(),
        "tracking must not create the app dir when not opted in"
    );

    // And nothing is opted in, so no event ever builds.
    assert!(!telemetry::is_opted_in());
    assert!(
        telemetry::build_cli_usage().is_none(),
        "no event when not opted in"
    );
}

/// Item A (#1877): the `telemetry.json` read-modify-write is serialized across
/// threads/processes, so a concurrent id-generation race can't lose an update.
/// Without the lock, barrier-synced threads each load an empty state, generate
/// distinct UUIDs, and return different ids (last-writer-wins); with it, the
/// first writer wins and every caller observes the same id.
#[test]
#[serial]
fn concurrent_ensure_install_id_yields_single_id() {
    let _tmp = isolate();

    const N: usize = 32;
    let barrier = Arc::new(Barrier::new(N));
    let handles: Vec<_> = (0..N)
        .map(|_| {
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                telemetry::ensure_install_id()
            })
        })
        .collect();

    let ids: Vec<Option<String>> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let first = ids[0].clone().expect("an id is generated");
    for (i, id) in ids.iter().enumerate() {
        assert_eq!(
            id.as_deref(),
            Some(first.as_str()),
            "thread {i} returned a different id; a concurrent RMW lost an update"
        );
    }
    assert_eq!(telemetry::install_id(), Some(first));
}

/// An unreachable / slow endpoint must never block the CLI: `track_cli_command`
/// (the per-invocation recorder + flush) is bounded and returns well within the
/// timeout even when the endpoint black-holes the connection.
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
    // Records "add" then flushes (due on a fresh install) against the dead
    // endpoint; the send is bounded so this returns fast.
    rt.block_on(telemetry::track_cli_command("add"));
    let elapsed = start.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "track_cli_command blocked for {elapsed:?}; must be bounded"
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
    let snapshot = telemetry::build_usage_snapshot(
        Surface::Serve,
        &[],
        usage_signals::zeroed(),
        0,
        None,
        None,
    )
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
    let snapshot = telemetry::build_usage_snapshot(
        Surface::Serve,
        &[],
        usage_signals::zeroed(),
        0,
        None,
        None,
    )
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

    let snapshot = telemetry::build_usage_snapshot(
        Surface::Serve,
        &[],
        usage_signals::zeroed(),
        0,
        None,
        None,
    )
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
        // A dotted fragment, so it can never collide with the dashless hex
        // install-id UUID (nor with any bucket string).
        assert!(
            !serialized.contains("1234.5678"),
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
    assert!(telemetry::build_usage_snapshot(
        Surface::Serve,
        &[],
        usage_signals::zeroed(),
        0,
        None,
        None
    )
    .is_none());
}
