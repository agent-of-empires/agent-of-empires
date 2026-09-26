//! A session's persisted model pick and pinned effort must be re-applied after
//! every handshake, not just on a fresh `session/new`.
//!
//! A worker respawn resumes the stored ACP session via `session/load`. The pick
//! travels to the adapter only as `AOE_AGENT_MODEL`, which claude-agent-acp
//! never reads: it resolves its model from `ANTHROPIC_MODEL`, `settings.model`
//! or the resumed transcript and re-asserts a settings pin inside
//! `session/load`, so every idle auto-stop, snooze expiry, daemon restart or
//! crash respawn silently dropped the pick. These tests drive the real
//! `AcpClient` against the test shim and assert the `session/set_config_option`
//! RPC fired with the pick on the load path and the fresh path, ran before the
//! effort, was skipped when the agent already reported the value, and did not
//! fail the spawn when the agent rejected it.

use std::path::PathBuf;
use std::time::Duration;

use agent_of_empires::acp::acp_client::{AcpClient, SpawnConfig};
use agent_of_empires::acp::agent_registry::AgentSpec;
use agent_of_empires::acp::state::{AcpSessionId, Event};

use crate::common::{shim_path, shim_ready};

fn spawn_config(
    shim: PathBuf,
    env: Vec<(String, String)>,
    stored_acp_session_id: Option<String>,
    default_model: Option<String>,
    default_effort: Option<String>,
) -> SpawnConfig {
    SpawnConfig {
        wrapper_substitution: None,
        agent_key: "claude".into(),
        tool: "claude".into(),
        spec: AgentSpec {
            command: crate::common::shim_node()
                .expect("shim prerequisite")
                .to_string_lossy()
                .into_owned(),
            args: vec![shim.to_string_lossy().to_string()],
            description: "model-option shim".into(),
            env_allowlist: None,
        },
        cwd: std::env::temp_dir(),
        additional_dirs: vec![],
        provider_env: env,
        host_environment: vec![],
        default_effort_explicit: default_effort.is_some(),
        default_effort,
        default_mode: None,
        default_model,
        socket_path: None,
        stored_acp_session_id,
        fork_from: None,
        seed_history_replay: false,
        generation: 0,
        artifact_dir: None,
        sandbox_info: None,
        source_profile: None,
        mcp_servers: Vec::new(),
        claude_store_pin: None,
    }
}

/// A prompt is only dispatched after the handshake completes, so a `Stopped`
/// proves any post-handshake config-option work already ran. Returns the
/// events seen before it.
async fn drive_one_turn(client: &mut AcpClient) -> Vec<Event> {
    client
        .send_prompt("hello", &[])
        .await
        .expect("send_prompt should reach the shim");
    let mut seen = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(200), client.next_event()).await {
            Ok(Some(Event::Stopped { .. })) => return seen,
            Ok(None) => panic!("ACP event stream closed before prompt completion"),
            Ok(Some(event)) => seen.push(event),
            Err(_) => continue,
        }
    }
    panic!("prompt did not complete after the handshake");
}

fn shim_env(
    record_path: &std::path::Path,
    load_session: bool,
    thought_level: bool,
) -> Vec<(String, String)> {
    let mut env = vec![
        ("SHIM_MODEL_OPTION".into(), "1".into()),
        (
            "SHIM_CONFIG_OPTION_RECORD_FILE".into(),
            record_path.to_string_lossy().to_string(),
        ),
    ];
    if load_session {
        env.push(("SHIM_LOAD_SESSION".into(), "1".into()));
    }
    if thought_level {
        env.push(("SHIM_THOUGHT_LEVEL".into(), "1".into()));
    }
    env
}

async fn run(config: SpawnConfig, label: &str) -> Vec<Event> {
    let mut client = AcpClient::spawn(config, AcpSessionId(label.into()))
        .await
        .expect("spawn shim");
    let events = drive_one_turn(&mut client).await;
    let _ = client.shutdown().await;
    events
}

/// The persisted model and pinned effort are applied after every establish
/// path: the respawn shape resumes via `session/load` (the agent advertises
/// `loadSession` and we hand it a stored id), where both picks used to be lost,
/// and `session/new` applies each exactly once. An unpinned session sends no
/// config-option RPC at all, so it keeps the agent's own defaults.
#[tokio::test]
#[serial_test::parallel]
async fn pinned_model_and_effort_applied_on_load_and_new() {
    if let Err(reason) = shim_ready() {
        eprintln!("skipping: {reason}");
        return;
    }
    // (label, resume via load, model, effort, expected record line, exact count)
    let cases = [
        (
            "model-load",
            true,
            Some("opus"),
            None,
            Some("model=opus"),
            None,
        ),
        (
            "model-new",
            false,
            Some("sonnet"),
            None,
            Some("model=sonnet"),
            Some(1),
        ),
        (
            "effort-load",
            true,
            None,
            Some("high"),
            Some("thought_level=high"),
            None,
        ),
        (
            "effort-new",
            false,
            None,
            Some("high"),
            Some("thought_level=high"),
            Some(1),
        ),
        ("unpinned", true, None, None, None, None),
    ];
    for (label, load, model, effort, line, exact) in cases {
        let temp = tempfile::tempdir().expect("tempdir");
        let record_path = temp.path().join("config-option-calls.log");
        let config = spawn_config(
            shim_path(),
            shim_env(&record_path, load, effort.is_some() || model.is_none()),
            (load && (model.is_some() || effort.is_some())).then(|| format!("stored-{label}")),
            model.map(Into::into),
            effort.map(Into::into),
        );
        run(config, label).await;

        let recorded = std::fs::read_to_string(&record_path).unwrap_or_default();
        let Some(line) = line else {
            assert!(recorded.trim().is_empty(), "{label}: {recorded:?}");
            continue;
        };
        let count = recorded.lines().filter(|l| *l == line).count();
        match exact {
            Some(exact) => assert_eq!(count, exact, "{label}: {recorded:?}"),
            None => assert!(count > 0, "{label}: {recorded:?}"),
        }
    }
}

/// When the handshake response already reports the persisted value, the
/// round-trip is skipped, so a resume with a matching model costs nothing extra.
#[tokio::test]
#[serial_test::parallel]
async fn pinned_model_skipped_when_already_current() {
    if let Err(reason) = shim_ready() {
        eprintln!("skipping: {reason}");
        return;
    }
    // Both rows in one run: "no model= line" is equally true of a session that
    // never reached the apply, or of the apply being gone, so the
    // differing-value row is what gives the equal-value row meaning. The
    // effort pin rides along as proof the post-handshake path ran at all.
    for (pin, expect_sent) in [("opus", true), ("default", false)] {
        let temp = tempfile::tempdir().expect("tempdir");
        let record_path = temp.path().join("config-option-calls.log");
        // The shim's initial `currentValue` is "default".
        let config = spawn_config(
            shim_path(),
            shim_env(&record_path, true, true),
            Some("stored-model-session".into()),
            Some(pin.into()),
            Some("high".into()),
        );
        run(config, "model-current").await;

        let recorded = std::fs::read_to_string(&record_path).unwrap_or_default();
        assert!(
            recorded.lines().any(|line| line == "thought_level=high"),
            "{pin}: the post-handshake apply path must have run (recorded: {recorded:?})"
        );
        assert_eq!(
            recorded.lines().any(|line| line == format!("model={pin}")),
            expect_sent,
            "{pin}: a model the agent already reports must not be re-sent, and one \
             it does not report must be (recorded: {recorded:?})"
        );
    }
}

/// A value the agent rejects (a stale alias after an upgrade, a pick persisted
/// from another agent's namespace) warns and never fails the spawn: the session
/// still comes up and answers, on the agent's own model, and the user is told.
#[tokio::test]
#[serial_test::parallel]
async fn rejected_model_does_not_fail_the_spawn() {
    if let Err(reason) = shim_ready() {
        eprintln!("skipping: {reason}");
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    let record_path = temp.path().join("config-option-calls.log");
    let config = spawn_config(
        shim_path(),
        shim_env(&record_path, true, false),
        Some("stored-model-session".into()),
        Some("no-such-model".into()),
        None,
    );
    // `run` panics if the spawn errors or the prompt never completes.
    let events = run(config, "model-rejected").await;

    let recorded = std::fs::read_to_string(&record_path).unwrap_or_default();
    assert!(
        recorded.lines().any(|line| line == "model=no-such-model"),
        "the re-assert must have been attempted (recorded: {recorded:?})"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::ConfigOptionSwitchFailed { config_id, value, .. }
                if config_id == "model" && value == "no-such-model"
        )),
        "a rejected re-assert must surface as a switch failure"
    );
}

/// A conversation reset re-applies the model the user last picked, not the
/// one the worker was spawned with.
#[tokio::test]
#[serial_test::parallel]
async fn reset_reapplies_the_live_model_pick() {
    if let Err(reason) = shim_ready() {
        eprintln!("skipping: {reason}");
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    let record_path = temp.path().join("config-option-calls.log");
    // The reset path re-reads the effort id from the model switch's response
    // exactly as the handshake does, so it is driven with an effort pin and the
    // renaming shim: the post-reset effort must land on the id that response
    // advertised, not the one the pre-reset option list carried.
    let mut env = shim_env(&record_path, false, true);
    env.push(("SHIM_RENUMBER_ON_MODEL".into(), "1".into()));
    let config = spawn_config(
        shim_path(),
        env,
        None,
        Some("sonnet".into()),
        Some("high".into()),
    );
    let mut client = AcpClient::spawn(config, AcpSessionId("model-reset".into()))
        .await
        .expect("spawn shim");
    drive_one_turn(&mut client).await;
    client.set_config_option("model", "opus").await.unwrap();
    client.reset_session("/clear").await.unwrap();
    let _ = client.shutdown().await;

    let recorded = std::fs::read_to_string(&record_path).unwrap_or_default();
    let models: Vec<&str> = recorded
        .lines()
        .filter(|line| line.starts_with("model="))
        .collect();
    assert_eq!(
        models,
        ["model=sonnet", "model=opus", "model=opus"],
        "a reset must re-apply the live pick, not the spawn-time model (recorded: {recorded:?})"
    );
    // `session/new` restores the default id, so the pre-reset rename is undone
    // and the reset's own switch renames it again: the last effort line is the
    // one the reset applied.
    assert_eq!(
        recorded
            .lines()
            .rfind(|line| line.starts_with("thought_level")),
        Some("thought_level_v2=high"),
        "the reset must resolve the effort id from its own model switch \
         (recorded: {recorded:?})"
    );
}

/// The reason the model is applied first, and the reason its response is read
/// back: an adapter may rebuild its option set around the switch. A client that
/// kept the establish-time id addresses an option that no longer exists, and
/// the effort pin is silently lost on every respawn. The shim's id is otherwise
/// fixed, so nothing else here would notice.
#[tokio::test]
#[serial_test::parallel]
async fn effort_id_is_re_read_from_the_model_switch_response() {
    if let Err(reason) = shim_ready() {
        eprintln!("skipping: {reason}");
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    let record_path = temp.path().join("config-option-calls.log");
    let mut env = shim_env(&record_path, true, true);
    env.push(("SHIM_RENUMBER_ON_MODEL".into(), "1".into()));
    let config = spawn_config(
        shim_path(),
        env,
        Some("stored-model-session".into()),
        Some("opus".into()),
        Some("high".into()),
    );
    run(config, "model-renumber").await;

    let recorded = std::fs::read_to_string(&record_path).expect("record file");
    assert!(
        recorded.lines().any(|line| line == "thought_level_v2=high"),
        "the effort must use the id the switch response advertised (recorded: {recorded:?})"
    );
    // The ordering this depends on, asserted here rather than in a second
    // session: the effort can only carry the post-switch id if the model went
    // first, so a reversal shows up as `thought_level=high` above.
    let lines: Vec<&str> = recorded.lines().collect();
    let model_at = lines.iter().position(|line| *line == "model=opus");
    let effort_at = lines
        .iter()
        .position(|line| line.starts_with("thought_level"));
    assert!(
        matches!((model_at, effort_at), (Some(m), Some(e)) if m < e),
        "model must be applied before the effort (recorded: {recorded:?})"
    );
}
