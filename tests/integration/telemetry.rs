//! Opt-in telemetry (#1762). Every test mutates process-global env to redirect
//! the app dir, so all are `#[serial]` against a fresh `TempDir`.

use agent_of_empires::session::{
    update_config, Instance, SandboxInfo, WorkspaceInfo, WorktreeInfo,
};
use agent_of_empires::telemetry::usage_signals::{self, UsageSeenCounters, USAGE_SIGNALS};
use agent_of_empires::telemetry::{self, Surface, UsageSnapshot};
use agent_of_empires::update::{ReleasesBehind, UpdateStatus};
use chrono::Utc;
use serial_test::serial;
use std::sync::{Arc, Barrier};
use std::time::Duration;

/// Redirect the app dir at a temp location and clear the telemetry env vars.
/// Keep the guard alive for the test's duration.
fn isolate() -> crate::common::TestHome {
    let mut home = crate::common::setup_temp_home();
    home.env = home.env.and_set("DO_NOT_TRACK", "");
    home.env = home.env.and_set("AOE_TELEMETRY_ENDPOINT", "");
    std::env::remove_var("DO_NOT_TRACK");
    std::env::remove_var("AOE_TELEMETRY_ENDPOINT");
    home
}

/// `isolate` plus an opt-in, which is what most stories start from.
fn opted_in() -> crate::common::TestHome {
    let home = isolate();
    update_config(|config| config.telemetry.enabled = true).expect("save config");
    telemetry::apply_opt_in_change(true);
    home
}

/// `build_usage_snapshot` with the arguments only a few tests vary.
fn build(
    surface: Surface,
    instances: &[Instance],
    signals: std::collections::BTreeMap<String, u32>,
    creates: u32,
    modes: (Option<&str>, Option<&str>),
    counts: &telemetry::StructuredInteractionCounts,
) -> Option<UsageSnapshot> {
    telemetry::build_usage_snapshot(
        surface, instances, signals, creates, modes.0, modes.1, counts,
    )
}

fn build_snapshot(surface: Surface, instances: &[Instance]) -> UsageSnapshot {
    build(
        surface,
        instances,
        usage_signals::zeroed(),
        0,
        (None, None),
        &telemetry::StructuredInteractionCounts::default(),
    )
    .expect("snapshot built when opted in")
}

/// Synthetic update-check cache so the version-health classifiers have
/// deterministic input. `releases` is newest-first, as the updater stores it.
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

fn with_sandbox(mut inst: Instance) -> Instance {
    inst.sandbox_info = Some(SandboxInfo {
        enabled: true,
        container_id: None,
        image: "secret-internal-image:latest".to_string(),
        container_name: "aoe_secret_container".to_string(),
        extra_env: None,
        custom_instruction: None,
        before_start_env: Vec::new(),
        container_workdir: None,
    });
    inst
}

/// The opt-in lifecycle. Default-off holds even with an update cache and
/// deployment modes in play: no opt-in, no install id, and no event (so no
/// uuid) is ever built. Opting in generates an id and lets events build;
/// opting back out deletes the id. `DO_NOT_TRACK` is absolute: with the config
/// flag on, nothing is opted in, no id is generated, and no events build.
#[test]
#[serial]
fn opt_in_lifecycle_and_do_not_track() {
    let _tmp = isolate();
    write_update_cache("9999.0.0", &["9999.0.0", "9998.0.0"]);

    assert!(!telemetry::is_opted_in());
    assert_eq!(telemetry::install_id(), None);
    assert!(telemetry::build_process_start(Surface::Cli).is_none());
    assert!(telemetry::build_cli_usage().is_none());
    assert!(build(
        Surface::Serve,
        &[],
        usage_signals::zeroed(),
        0,
        (Some("none"), Some("tunnel")),
        &telemetry::StructuredInteractionCounts::default(),
    )
    .is_none());

    update_config(|config| config.telemetry.enabled = true).expect("save config");
    telemetry::apply_opt_in_change(true);
    assert!(telemetry::is_opted_in());
    let id = telemetry::install_id().expect("id generated on opt-in");
    assert!(!id.is_empty());
    let event = telemetry::build_process_start(Surface::Tui).expect("event built when opted in");
    assert_eq!(event.surface, Surface::Tui);
    assert_eq!(event.event, "process_start");
    assert_eq!(event.install_id, id);

    update_config(|config| config.telemetry.enabled = false).expect("save config");
    telemetry::apply_opt_in_change(false);
    assert!(!telemetry::is_opted_in());
    assert_eq!(telemetry::install_id(), None);
    assert!(telemetry::build_process_start(Surface::Tui).is_none());

    update_config(|config| config.telemetry.enabled = true).expect("save config");
    unsafe { std::env::set_var("DO_NOT_TRACK", "1") };
    assert!(telemetry::do_not_track());
    assert!(!telemetry::is_opted_in());
    telemetry::apply_opt_in_change(true);
    assert_eq!(telemetry::install_id(), None, "suppressed: no id");
    assert!(telemetry::build_process_start(Surface::Cli).is_none());
    unsafe { std::env::remove_var("DO_NOT_TRACK") };
}

/// The snapshot payload carries only allowlisted buckets: a custom agent
/// command and project path collapse to `custom`, never the raw strings.
#[test]
#[serial]
fn snapshot_buckets_are_sanitized() {
    let _tmp = opted_in();
    let mut custom = Instance::new("secret-session", "/home/me/secret-project");
    custom.tool = "/usr/local/bin/my-internal-agent".to_string();
    custom.detect_as = String::new();

    let snapshot = build_snapshot(Surface::Tui, &[custom, Instance::new("c", "/p")]);
    let serialized = serde_json::to_string(&snapshot).expect("serialize");
    for raw in ["my-internal-agent", "secret-project", "secret-session"] {
        assert!(!serialized.contains(raw), "{raw} leaked: {serialized}");
    }
    // The TUI surface has no serve deployment mode, so those fields are omitted.
    assert!(snapshot.auth_mode.is_none());
    assert!(snapshot.serve_mode.is_none());
    assert!(!serialized.contains("auth_mode"));
    assert!(!serialized.contains("serve_mode"));

    assert_eq!(snapshot.sessions_by_agent.get("custom"), Some(&1));
    assert_eq!(snapshot.sessions_by_agent.get("claude"), Some(&1));
    assert_eq!(snapshot.session_total, 2);

    // The base builder leaves the serve-window fields at their defaults; only
    // `aoe serve` overrides them from its aggregator.
    assert_eq!(snapshot.peak_concurrent_sessions, 2);
    assert!(snapshot.distinct_sessions_by_agent.is_empty());
    assert!(snapshot.distinct_sessions_by_model_bucket.is_empty());

    for key in ["worktree", "sandbox", "auto_update"] {
        assert!(
            snapshot.features.contains_key(key),
            "features map missing allowlisted key `{key}`"
        );
    }
}

/// #1883: the per-class client form-factor maps are a presence set on the wire.
/// Only seen classes appear (as `true`), and an empty map is omitted rather
/// than serialized as `{}`.
#[test]
#[serial]
fn form_factor_maps_are_a_presence_set_on_the_wire() {
    let _tmp = opted_in();
    let mut snapshot = build_snapshot(Surface::Serve, &[]);
    let empty_wire = serde_json::to_string(&snapshot).expect("serialize");
    for field in ["web_clients_seen", "structured_clients_seen"] {
        assert!(
            !empty_wire.contains(field),
            "empty {field} must be skipped, not emitted as {{}}"
        );
    }

    snapshot
        .web_clients_seen
        .insert("desktop".to_string(), true);
    snapshot
        .web_clients_seen
        .insert("mobile_pwa".to_string(), true);
    assert_eq!(snapshot.web_clients_seen.get("mobile_pwa"), Some(&true));
    assert_eq!(snapshot.web_clients_seen.get("mobile"), None);
    let wire = serde_json::to_string(&snapshot).expect("serialize");
    assert!(wire.contains("web_clients_seen") && wire.contains("mobile_pwa"));
}

/// #1886: the substrate census counts each session exactly once, by the
/// documented precedence, into the closed vocabulary, and the buckets partition
/// `session_total`. No path, repo name, branch, or image string rides along.
#[test]
#[serial]
fn substrate_census_partitions_sessions_into_allowlisted_buckets() {
    let _tmp = opted_in();
    // Impossible-but-defensive combo: scratch AND worktree. Precedence puts it
    // in `scratch`. A sandboxed worktree buckets as `worktree` (sandbox sits
    // below it) but still increments the orthogonal sandbox count.
    let mut conflicted = with_worktree(Instance::new("a", "/home/me/secret-project"));
    conflicted.scratch = true;
    let instances = [
        conflicted,
        with_sandbox(with_worktree(Instance::new("b", "/p"))),
        with_workspace(Instance::new("c", "/home/me/secret-workspace")),
        Instance::new("d", "/p"),
        with_sandbox(Instance::new("e", "/p")),
    ];
    let total = instances.len() as u32;
    let snapshot = build_snapshot(Surface::Serve, &instances);

    let sum: u32 = snapshot.sessions_by_substrate.values().sum();
    assert_eq!(
        sum, total,
        "substrate buckets must partition session_total exactly once each"
    );
    assert_eq!(snapshot.session_total, total);
    for (bucket, expected) in [
        ("scratch", 1),
        ("worktree", 1),
        ("workspace", 1),
        ("local", 1),
        ("sandbox", 1),
    ] {
        assert_eq!(
            snapshot.sessions_by_substrate.get(bucket),
            Some(&expected),
            "{bucket}"
        );
    }
    assert_eq!(snapshot.session_sandboxed, 2, "orthogonal to the buckets");

    const VOCAB: [&str; 5] = ["local", "worktree", "workspace", "sandbox", "scratch"];
    for key in snapshot.sessions_by_substrate.keys() {
        assert!(
            VOCAB.contains(&key.as_str()),
            "substrate key `{key}` is outside the closed vocabulary"
        );
    }
    let serialized = serde_json::to_string(&snapshot).expect("serialize");
    for raw in [
        "secret-project",
        "secret-workspace",
        "secret-internal-image",
    ] {
        assert!(!serialized.contains(raw), "{raw} leaked: {serialized}");
    }
}

/// #1874 and #1873: the create-trend counter carries its real value, and every
/// event gets its own non-empty `uuid` (the gateway's dedup key), distinct from
/// `install_id`, `sent_at`, and any other event's.
#[test]
#[serial]
fn events_carry_create_count_and_distinct_idempotency_uuid() {
    let _tmp = opted_in();
    let counts = telemetry::StructuredInteractionCounts::default();
    let with_creates = build(
        Surface::Serve,
        &[],
        usage_signals::zeroed(),
        7,
        (None, None),
        &counts,
    )
    .expect("snapshot built when opted in");
    assert_eq!(with_creates.session_creates_since_last_snapshot, 7);

    let snap = build_snapshot(Surface::Tui, &[]);
    assert_eq!(snap.session_creates_since_last_snapshot, 0);
    assert!(!snap.uuid.is_empty());
    assert_ne!(snap.uuid, snap.install_id);
    assert_ne!(snap.uuid, snap.sent_at);
    let serialized = serde_json::to_string(&snap).expect("serialize");
    assert!(
        serialized.contains(&format!("\"uuid\":\"{}\"", snap.uuid)),
        "the gateway reads the uuid off the wire"
    );

    let proc = telemetry::build_process_start(Surface::Tui).expect("process_start built");
    let snap2 = build_snapshot(Surface::Tui, &[]);
    assert_ne!(snap.uuid, snap2.uuid);
    assert_ne!(proc.uuid, snap.uuid);
}

/// #1880 and #1881: registered usage signals (whole-UI opens and dashboard
/// features alike) flow through the daemon aggregate into `usage_seen`
/// verbatim, an unregistered name is rejected by the registry (the endpoint
/// turns that into a 400), and the map's keys are exactly the registry.
#[test]
#[serial]
fn usage_seen_carries_registered_signals_only() {
    let _tmp = opted_in();
    let counters = UsageSeenCounters::new();
    for signal in ["web", "web", "structured_view", "diff_comments"] {
        assert!(counters.record(signal), "{signal} is registered");
    }
    assert!(!counters.record("not_a_signal"), "off the allowlist");

    let snapshot = build(
        Surface::Serve,
        &[],
        counters.snapshot(),
        0,
        (None, None),
        &telemetry::StructuredInteractionCounts::default(),
    )
    .expect("snapshot built when opted in");
    assert_eq!(snapshot.usage_seen.get("web"), Some(&2));
    assert_eq!(snapshot.usage_seen.get("structured_view"), Some(&1));
    assert_eq!(snapshot.usage_seen.get("diff_comments"), Some(&1));
    assert!(!snapshot.usage_seen.contains_key("not_a_signal"));

    // `usage_seen` is a BTreeMap, so compare against the registry sorted the
    // same way rather than relying on its source order.
    let keys: Vec<&str> = snapshot.usage_seen.keys().map(String::as_str).collect();
    let mut expected: Vec<&str> = USAGE_SIGNALS.to_vec();
    expected.sort_unstable();
    assert_eq!(keys, expected);
    for key in keys {
        assert!(
            key.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
            "usage_seen key `{key}` is not a short allowlisted identifier"
        );
    }
}

/// #1885: the serve snapshot carries the coarse deployment mode from its closed
/// allowlist, never a tunnel name, hostname, token, or passphrase.
#[test]
#[serial]
fn serve_snapshot_carries_coarse_deployment_mode() {
    let _tmp = opted_in();
    let counts = telemetry::StructuredInteractionCounts::default();
    for (auth, serve) in [("passphrase", "tailscale"), ("token", "local")] {
        let snapshot = build(
            Surface::Serve,
            &[],
            usage_signals::zeroed(),
            0,
            (Some(auth), Some(serve)),
            &counts,
        )
        .expect("snapshot built when opted in");
        assert_eq!(snapshot.auth_mode.as_deref(), Some(auth));
        assert_eq!(snapshot.serve_mode.as_deref(), Some(serve));
        let serialized = serde_json::to_string(&snapshot).expect("serialize");
        assert!(serialized.contains(&format!("\"auth_mode\":\"{auth}\"")));
        assert!(serialized.contains(&format!("\"serve_mode\":\"{serve}\"")));
    }
}

/// #1888: structured-interaction signals fold into the snapshot as counts and a
/// closed decision-key set. No prompt text, tool name, file path, or agent
/// command can ride along.
#[test]
#[serial]
fn snapshot_carries_acp_interaction_counts() {
    let _tmp = opted_in();
    let counts = telemetry::StructuredInteractionCounts {
        approvals_allow: 2,
        approvals_allow_always: 0,
        approvals_deny: 1,
        agent_switches: 1,
        plan_mode_seen: true,
        prompts_queued: 1,
    };
    let snap = build(
        Surface::Serve,
        &[],
        usage_signals::zeroed(),
        0,
        (None, None),
        &counts,
    )
    .expect("snapshot built when opted in");

    assert_eq!(snap.approvals_resolved, 3);
    assert_eq!(snap.approvals_by_decision.get("allow"), Some(&2));
    assert_eq!(snap.approvals_by_decision.get("deny"), Some(&1));
    assert!(
        !snap.approvals_by_decision.contains_key("allow_always"),
        "zero decisions stay out of the map"
    );
    assert_eq!(snap.agent_switches, 1);
    assert!(snap.plan_mode_seen);
    assert_eq!(snap.prompts_queued, 1);
    assert_eq!(snap.schema, telemetry::SCHEMA_VERSION);

    let json: serde_json::Value = serde_json::to_value(&snap).expect("snapshot serializes");
    let obj = json.as_object().expect("snapshot is a JSON object");
    for field in ["approvals_resolved", "agent_switches", "prompts_queued"] {
        assert!(
            obj.get(field).and_then(serde_json::Value::as_u64).is_some(),
            "`{field}` must serialize as a count"
        );
    }
    assert!(obj
        .get("plan_mode_seen")
        .and_then(serde_json::Value::as_bool)
        .is_some());
    for (key, value) in obj["approvals_by_decision"]
        .as_object()
        .expect("decision map is an object")
    {
        assert!(
            matches!(key.as_str(), "allow" | "allow_always" | "deny"),
            "unexpected decision key `{key}`"
        );
        assert!(value.as_u64().is_some(), "decision counts must be numeric");
    }
}

/// #1875: the `cli_usage` flush is throttled to once per install per day, and a
/// failed send stamps the attempt without claiming the daily slot, so the next
/// invocation retries once the retry gap elapses.
#[test]
#[serial]
fn cli_usage_flush_throttled_but_retries_after_failure() {
    let _tmp = opted_in();
    let day = Duration::from_secs(24 * 60 * 60);
    let hour = Duration::from_secs(60 * 60);

    assert!(telemetry::cli_usage_due(day, hour), "first send is due");
    telemetry::record_cli_usage_flush(true);
    assert!(
        !telemetry::cli_usage_due(day, hour),
        "a confirmed send claims the daily slot"
    );
    // Zero gaps always re-grant: every stamp is older than zero.
    assert!(telemetry::cli_usage_due(Duration::ZERO, Duration::ZERO));
    drop(_tmp);

    // A fresh install: the daily slot is unclaimed, so a *failed* send must
    // leave it open even though the attempt stamp throttles the retry.
    let _tmp = opted_in();
    telemetry::record_cli_usage_flush(false);
    assert!(
        !telemetry::cli_usage_due(day, hour),
        "the retry gap blocks an immediate re-attempt after a failed send"
    );
    assert!(
        telemetry::cli_usage_due(day, Duration::ZERO),
        "a failed send must leave the daily slot open for retry"
    );
}

/// #1879: for a not-opted-in install the per-command tracker is a true no-op,
/// checked before any config read since reading the config materializes the
/// app dir (`track_cli_command` short-circuits on its `app_dir_exists` gate).
/// Once opted in, `cli_usage` reports the full mix of allowlisted command
/// names with repeats, drops anything outside the clap vocabulary (so a
/// hand-edited state file cannot smuggle strings onto the wire), and resets on
/// a confirmed flush.
#[test]
#[serial]
fn cli_usage_records_allowlisted_subcommands_only_when_opted_in() {
    let _tmp = isolate();
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    rt.block_on(telemetry::track_cli_command("add"));
    assert!(
        !agent_of_empires::session::app_dir_exists(),
        "tracking must not create the app dir when not opted in"
    );
    assert!(!telemetry::is_opted_in());
    assert!(telemetry::build_cli_usage().is_none());

    update_config(|config| config.telemetry.enabled = true).expect("save config");
    telemetry::apply_opt_in_change(true);
    telemetry::record_cli_command("add");
    telemetry::record_cli_command("session");
    telemetry::record_cli_command("add");
    telemetry::record_cli_command("/home/me/secret-project");

    let event = telemetry::build_cli_usage().expect("event built when opted in with counts");
    assert_eq!(event.event, "cli_usage");
    assert_eq!(event.surface, Surface::Cli);
    assert_eq!(event.command_counts.get("add"), Some(&2));
    assert_eq!(event.command_counts.get("session"), Some(&1));
    assert_eq!(event.command_counts.len(), 2);
    assert!(!event.window_start.is_empty());
    for key in event.command_counts.keys() {
        assert!(
            agent_of_empires::cli::CLI_COMMAND_NAMES.contains(&key.as_str()),
            "non-allowlisted key `{key}` leaked into command_counts"
        );
    }
    assert!(!serde_json::to_string(&event)
        .expect("serialize")
        .contains("secret-project"));

    telemetry::record_cli_usage_flush(true);
    assert!(
        telemetry::build_cli_usage().is_none(),
        "counts must reset after a confirmed flush"
    );
}

/// #1877: the `telemetry.json` read-modify-write is serialized, so racing
/// id generations all observe the first writer's id instead of last-writer-wins.
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

/// A dead endpoint must never block the CLI: the per-invocation recorder plus
/// flush is bounded.
#[test]
#[serial]
fn unreachable_endpoint_never_blocks() {
    let _tmp = opted_in();
    // Nothing listens on the discard port; the bound is what guarantees this.
    unsafe { std::env::set_var("AOE_TELEMETRY_ENDPOINT", "http://127.0.0.1:9/ingest") };

    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let start = std::time::Instant::now();
    rt.block_on(telemetry::track_cli_command("add"));
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(5),
        "track_cli_command blocked for {elapsed:?}; must be bounded"
    );

    unsafe { std::env::remove_var("AOE_TELEMETRY_ENDPOINT") };
}

/// #1887: events carry the data-schema version and coarse version-health
/// buckets derived from the cached update check. With no cache the staleness is
/// Unknown (never a false "current"), and the cached version string itself
/// never reaches the wire.
#[test]
#[serial]
fn version_health_reports_coarse_buckets_only() {
    let _tmp = opted_in();
    let event = telemetry::build_process_start(Surface::Cli).expect("event built when opted in");
    assert_eq!(
        event.data_schema_version,
        agent_of_empires::migrations::current_schema_version()
    );
    assert_eq!(event.update_status, UpdateStatus::Unknown);
    assert_eq!(event.update_releases_behind, ReleasesBehind::Unknown);

    // Two cached releases newer than any plausible build.
    let secret_latest = "9999.1234.5678";
    write_update_cache(secret_latest, &[secret_latest, "9998.0.0"]);
    let snapshot = build_snapshot(Surface::Serve, &[]);
    assert_eq!(snapshot.update_status, UpdateStatus::MajorBehind);
    assert_eq!(
        snapshot.update_releases_behind,
        ReleasesBehind::SeveralBehind
    );
    let event = telemetry::build_process_start(Surface::Cli).expect("event built when opted in");
    assert_eq!(event.update_status, UpdateStatus::MajorBehind);
    assert_eq!(event.update_releases_behind, ReleasesBehind::SeveralBehind);

    for serialized in [
        serde_json::to_string(&snapshot).expect("serialize snapshot"),
        serde_json::to_string(&event).expect("serialize process_start"),
    ] {
        assert!(
            !serialized.contains(secret_latest) && !serialized.contains("1234.5678"),
            "the cached version leaked into the payload: {serialized}"
        );
        assert!(
            serialized.contains("major_behind"),
            "only the coarse bucket leaves the client: {serialized}"
        );
    }

    write_update_cache("9999.0.0", &["9999.0.0"]);
    assert_eq!(
        build_snapshot(Surface::Serve, &[]).update_releases_behind,
        ReleasesBehind::OneBehind
    );
}
