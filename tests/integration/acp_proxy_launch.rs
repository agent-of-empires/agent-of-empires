//! A plugin proxy launched ahead of the adapter: the runner passes the proxy's
//! argv through intact and hands it an owner-only launch config that is gone
//! once the session ends. Spawns a real `aoe __acp-runner` over the fake proxy
//! (`acp-worker/test-shim/fake-proxy.mjs`) wrapping the Node shim.

use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use agent_of_empires::process::worker_registry;
use serial_test::serial;

use crate::common::{set_temp_home, shim_node, shim_path, shim_ready};

const PROXY_SPEC_JSON: &str = r#"{"plugin_id":"fake-proxy","command":"p","args":[],"failure_policy":"neutral_boot","display_name":"Fake","compat":{"agent_names":[],"version_req":null}}"#;

fn wait_for(path: &Path, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(Instant::now() < deadline, "{what} never appeared");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
#[serial]
fn proxy_receives_argv_and_owner_only_launch_config_removed_at_session_end() {
    if let Err(reason) = shim_ready() {
        eprintln!("skipping: {reason}");
        return;
    }
    let temp = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let _home = set_temp_home(temp.path());
    let record_dir = temp.path().join("record");
    std::fs::create_dir_all(&record_dir).unwrap();

    let session_id = "proxy-launch";
    let launch_config = worker_registry::write_proxy_launch_config(session_id, b"{}").unwrap();
    assert_eq!(
        std::fs::metadata(&launch_config)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );

    let node = shim_node().expect("shim prerequisite");
    let proxy = shim_path().with_file_name("fake-proxy.mjs");
    let socket = worker_registry::socket_path_for(session_id).unwrap();
    let mut runner = Command::new(env!("CARGO_BIN_EXE_aoe"))
        .args(["__acp-runner", "--socket"])
        .arg(&socket)
        .args(["--session-id", session_id, "--agent-name", "shim", "--cwd"])
        .arg(temp.path())
        .args(["--proxy", PROXY_SPEC_JSON, "--degraded", "Fake"])
        .args(["--", node.to_str().unwrap(), proxy.to_str().unwrap()])
        .args(["--flag", "a b", "it's \"quoted\"", "--"])
        .args([node.to_str().unwrap(), shim_path().to_str().unwrap()])
        .env("FAKE_PROXY_RECORD_DIR", &record_dir)
        .env("AOE_PROXY_LAUNCH_CONFIG", &launch_config)
        .spawn()
        .expect("spawn acp runner");

    wait_for(&record_dir.join("env.json"), "proxy env record");
    let argv: Vec<String> =
        serde_json::from_slice(&std::fs::read(record_dir.join("argv.json")).unwrap()).unwrap();
    assert_eq!(
        argv,
        [
            "--flag",
            "a b",
            "it's \"quoted\"",
            "--",
            node.to_str().unwrap(),
            shim_path().to_str().unwrap()
        ]
    );
    let env: serde_json::Value =
        serde_json::from_slice(&std::fs::read(record_dir.join("env.json")).unwrap()).unwrap();
    assert_eq!(
        env["AOE_PROXY_LAUNCH_CONFIG"],
        launch_config.to_str().unwrap()
    );
    assert!(launch_config.exists(), "config must outlive launch");
    let record = worker_registry::load(session_id)
        .unwrap()
        .expect("runner record");
    assert_eq!(
        record.proxy.expect("proxy persisted").plugin_id,
        "fake-proxy"
    );
    assert_eq!(record.degraded.as_deref(), Some("Fake"));
    assert_eq!(
        std::fs::metadata(&launch_config)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );

    // The proxy exiting ends the session.
    let pid = std::fs::read_to_string(record_dir.join("pid")).unwrap();
    assert!(Command::new("kill")
        .args(["-TERM", pid.trim()])
        .status()
        .unwrap()
        .success());
    let deadline = Instant::now() + Duration::from_secs(10);
    while runner.try_wait().unwrap().is_none() {
        if Instant::now() > deadline {
            let _ = runner.kill();
            panic!("runner did not exit after its proxy did");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !launch_config.exists(),
        "config must be removed at session end"
    );
}

fn pid_alive(pid: &str) -> bool {
    Command::new("kill")
        .args(["-0", pid])
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success()
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Runs a runner (its own group leader, as the daemon spawns it) over the fake
/// proxy and a `sleep` adapter (unlike the shim, it ignores stdin EOF), ends it
/// with `end`, and asserts neither the proxy nor the adapter outlives it.
fn assert_no_orphans(session_id: &str, proxy_mode: &str, end: impl FnOnce(&str, &str)) {
    let temp = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let _home = set_temp_home(temp.path());
    let record_dir = temp.path().join("record");
    std::fs::create_dir_all(&record_dir).unwrap();

    let node = shim_node().expect("shim prerequisite");
    let proxy = shim_path().with_file_name("fake-proxy.mjs");
    let socket = worker_registry::socket_path_for(session_id).unwrap();
    let mut runner = Command::new(env!("CARGO_BIN_EXE_aoe"))
        .args(["__acp-runner", "--socket"])
        .arg(&socket)
        .args(["--session-id", session_id, "--agent-name", "shim", "--cwd"])
        .arg(temp.path())
        .args(["--", node.to_str().unwrap(), proxy.to_str().unwrap()])
        .args(["--", "sleep", "600"])
        .env("FAKE_PROXY_RECORD_DIR", &record_dir)
        .env("FAKE_PROXY_MODE", proxy_mode)
        .process_group(0)
        .spawn()
        .expect("spawn acp runner");

    wait_for(&record_dir.join("adapter_pid"), "adapter pid record");
    let read = |name: &str| std::fs::read_to_string(record_dir.join(name)).unwrap();
    let (proxy_pid, adapter_pid) = (read("pid"), read("adapter_pid"));
    assert!(pid_alive(&proxy_pid) && pid_alive(&adapter_pid));

    end(&runner.id().to_string(), proxy_pid.trim());

    wait_until("runner did not exit", || {
        runner.try_wait().unwrap().is_some()
    });
    wait_until("proxy outlived the runner", || !pid_alive(&proxy_pid));
    wait_until("adapter outlived the runner", || !pid_alive(&adapter_pid));
}

fn signal(sig: &str, pid: &str) {
    assert!(Command::new("kill")
        .args([sig, pid])
        .status()
        .unwrap()
        .success());
}

#[test]
#[serial]
fn runner_shutdown_leaves_no_orphan_proxy_or_adapter() {
    if let Err(reason) = shim_ready() {
        eprintln!("skipping: {reason}");
        return;
    }
    assert_no_orphans("proxy-orphan-term", "orphan-on-term", |runner, _| {
        signal("-TERM", runner)
    });
}

#[test]
#[serial]
fn proxy_exit_leaves_no_orphan_adapter() {
    if let Err(reason) = shim_ready() {
        eprintln!("skipping: {reason}");
        return;
    }
    assert_no_orphans("proxy-orphan-exit", "ok", |_, proxy| signal("-KILL", proxy));
}

#[cfg(debug_assertions)]
mod attach {
    use agent_of_empires::acp::acp_client::{AcpClient, AcpError, ProxyFailurePolicy, ProxySpec};
    use agent_of_empires::acp::agent_compat::ProxyCompat;
    use agent_of_empires::acp::state::{AcpSessionId, Event, StartupErrorDetail};

    use crate::common::{shim_node, shim_path, shim_ready, spawn_runner, RunnerGuard};
    use std::time::Duration;

    fn proxy_spec() -> ProxySpec {
        ProxySpec {
            plugin_id: "fake-proxy".into(),
            command: String::new(),
            args: Vec::new(),
            failure_policy: ProxyFailurePolicy::Refuse,
            display_name: "Fake".into(),
            compat: ProxyCompat::default(),
        }
    }

    fn proxy_argv() -> [String; 5] {
        let node = shim_node().expect("shim prerequisite").to_str().unwrap();
        let proxy = shim_path().with_file_name("fake-proxy.mjs");
        [
            node,
            proxy.to_str().unwrap(),
            "--",
            node,
            shim_path().to_str().unwrap(),
        ]
        .map(String::from)
    }

    async fn connect(
        socket: std::path::PathBuf,
        session_id: &str,
        acp_session_id: &str,
    ) -> Result<AcpClient, AcpError> {
        AcpClient::attach(
            socket,
            std::env::temp_dir(),
            vec![],
            acp_session_id.into(),
            false,
            AcpSessionId(session_id.into()),
            None,
            "shim".into(),
            None,
            None,
            None,
            None,
            Some(proxy_spec()),
            false,
        )
        .await
    }

    async fn attach(session_id: &str, mode: &str) -> (Result<AcpClient, AcpError>, RunnerGuard) {
        let env = [("FAKE_PROXY_MODE", mode.to_string())];
        let (socket, runner) = spawn_runner(session_id, &env, &proxy_argv()).await;
        (connect(socket, session_id, "unused").await, runner)
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn a_good_hello_connects() {
        if let Err(reason) = shim_ready() {
            eprintln!("skipping: {reason}");
            return;
        }
        let (client, _runner) = attach("proxy-attach-ok", "ok").await;
        let mut client = client.expect("attach");
        let assigned = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while let Some(event) = client.next_event().await {
                if matches!(event, Event::AcpSessionAssigned { .. }) {
                    return true;
                }
            }
            false
        })
        .await
        .expect("timed out");
        assert!(assigned, "a proxied session must reach AcpSessionAssigned");
    }

    /// Writes the proxy's own frames: `(text, after_adapter_sequence)`, where
    /// `None` is an adapter frame.
    fn emit(file: &std::path::Path, acp_session_id: &str, frames: &[(&str, Option<u64>)]) {
        let lines: String = frames
            .iter()
            .map(|(text, after)| {
                let mut update = serde_json::json!({
                    "sessionUpdate": "agent_message_chunk",
                    "content": {"type": "text", "text": text},
                });
                if let Some(after) = after {
                    update["_meta"] = serde_json::json!({
                        "aoe": {"v": 1, "key": text, "after_adapter_sequence": after}
                    });
                }
                let frame = serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "session/update",
                    "params": {"sessionId": acp_session_id, "update": update},
                });
                format!("{frame}\n")
            })
            .collect();
        let staged = file.with_extension("staged");
        std::fs::write(&staged, lines).unwrap();
        std::fs::rename(staged, file).unwrap();
    }

    async fn next_chunk(client: &mut AcpClient) -> String {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Event::AgentMessageChunk { text, .. } =
                    client.next_event().await.expect("event stream open")
                {
                    return text;
                }
            }
        })
        .await
        .expect("the expected frame never arrived")
    }

    async fn assigned_id(client: &mut AcpClient) -> String {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Event::AcpSessionAssigned { acp_session_id } =
                    client.next_event().await.expect("event stream open")
                {
                    return acp_session_id;
                }
            }
        })
        .await
        .expect("session never assigned")
    }

    /// The proxy numbers adapter frames for the life of the adapter, but a
    /// daemon counts per connection. After a reattach a frame anchored before
    /// it must still be released at the right adapter frame, and a replayed
    /// backlog must not be counted twice.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_reattach_keeps_counting_the_adapters_frames() {
        if let Err(reason) = shim_ready() {
            eprintln!("skipping: {reason}");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("emit");
        let env = [("FAKE_PROXY_EMIT_FILE", file.display().to_string())];
        let id = "proxy-reattach-count";
        let (socket, _runner) = spawn_runner(id, &env, &proxy_argv()).await;

        let mut client = connect(socket.clone(), id, "unused").await.expect("attach");
        let acp = assigned_id(&mut client).await;
        emit(&file, &acp, &[("a1", None), ("a2", None)]);
        assert_eq!(next_chunk(&mut client).await, "a1");
        assert_eq!(next_chunk(&mut client).await, "a2");
        client.shutdown().await.unwrap();

        // Quiet adapter: an anchor behind the count is delivered at once.
        let mut client = connect(socket.clone(), id, &acp).await.expect("reattach");
        assigned_id(&mut client).await;
        emit(&file, &acp, &[("t2", Some(2))]);
        assert_eq!(next_chunk(&mut client).await, "t2");

        // An anchor ahead of the count waits for exactly the next adapter frame.
        emit(&file, &acp, &[("t3", Some(3)), ("a3", None)]);
        assert_eq!(next_chunk(&mut client).await, "a3");
        assert_eq!(next_chunk(&mut client).await, "t3");
        client.shutdown().await.unwrap();

        // Frames sent while detached are replayed and counted once.
        emit(&file, &acp, &[("a4", None), ("t5", Some(5))]);
        let mut client = connect(socket, id, &acp).await.expect("second reattach");
        assigned_id(&mut client).await;
        assert_eq!(next_chunk(&mut client).await, "a4");
        emit(&file, &acp, &[("a5", None)]);
        assert_eq!(next_chunk(&mut client).await, "a5");
        assert_eq!(next_chunk(&mut client).await, "t5");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn a_bad_or_missing_hello_is_refused() {
        if let Err(reason) = shim_ready() {
            eprintln!("skipping: {reason}");
            return;
        }
        for (mode, id) in [
            ("bad-hello", "proxy-attach-bad"),
            ("no-hello", "proxy-attach-none"),
        ] {
            let (result, _runner) = attach(id, mode).await;
            match result {
                Err(AcpError::IncompatibleAgent(error)) => assert!(
                    matches!(error.detail, StartupErrorDetail::InvalidProxyHello { .. }),
                    "{mode}: {:?}",
                    error.detail
                ),
                Err(other) => panic!("{mode}: unexpected error {other}"),
                Ok(_) => panic!("{mode}: a bad hello must not connect"),
            }
        }
    }
}

/// Killing a proxied session's proxy mid-turn loses its integration rather
/// than failing it as a startup error.
#[cfg(debug_assertions)]
#[tokio::test]
#[serial]
async fn killing_the_proxy_mid_turn_loses_the_integration() {
    use agent_of_empires::acp::acp_client::{
        AcpClient, ProxyFailurePolicy, ProxySpec, SpawnConfig,
    };
    use agent_of_empires::acp::agent_compat::ProxyCompat;
    use agent_of_empires::acp::agent_registry::AgentSpec;
    use agent_of_empires::acp::state::{AcpSessionId, Event, IntegrationLostReason};

    if let Err(reason) = shim_ready() {
        eprintln!("skipping: {reason}");
        return;
    }
    let temp = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
    let record_dir = temp.path().join("record");
    std::fs::create_dir_all(&record_dir).unwrap();
    let node = shim_node().expect("shim prerequisite");
    let proxy = shim_path().with_file_name("fake-proxy.mjs");
    let cwd = temp.path().to_path_buf();
    let config = SpawnConfig {
        launch_lease: None,
        provider_routing: Vec::new(),
        wrapper_substitution: None,
        agent_key: "claude".into(),
        tool: "claude".into(),
        spec: AgentSpec {
            command: node.to_string_lossy().into_owned(),
            args: vec![
                proxy.to_string_lossy().into_owned(),
                "--".into(),
                node.to_string_lossy().into_owned(),
                shim_path().to_string_lossy().into_owned(),
            ],
            description: "fake proxy over the test shim".into(),
            env_allowlist: None,
        },
        config_path: cwd.clone(),
        cwd,
        additional_dirs: vec![],
        provider_env: vec![],
        host_environment: vec![
            (
                "FAKE_PROXY_RECORD_DIR".into(),
                record_dir.to_string_lossy().into_owned(),
            ),
            // Holds the prompt open until a release file that never appears.
            (
                "SHIM_PROMPT_COMPLETION_RELEASE_FILE".into(),
                temp.path().join("never").to_string_lossy().into_owned(),
            ),
        ],
        default_effort: None,
        default_effort_explicit: false,
        default_model_config: Default::default(),
        default_mode: None,
        default_model: None,
        socket_path: None,
        stored_acp_session_id: None,
        fork_from: None,
        seed_history_replay: false,
        prior_conversation: false,
        generation: 0,
        artifact_dir: None,
        proxy: Some(ProxySpec {
            plugin_id: "fake-proxy".into(),
            command: String::new(),
            args: Vec::new(),
            failure_policy: ProxyFailurePolicy::Refuse,
            display_name: "Fake".into(),
            compat: ProxyCompat::default(),
        }),
        degraded: None,
        sandbox_info: None,
        workspace_info: None,
        source_profile: None,
        mcp_servers: Vec::new(),
        claude_store_pin: None,
        base_host_environment: vec![],
    };
    let mut client = AcpClient::spawn(config, AcpSessionId("proxy-mid-turn".into()))
        .await
        .expect("spawn fake proxy");
    client.send_prompt("hello", &[]).await.expect("send_prompt");

    let lost = tokio::time::timeout(Duration::from_secs(20), async {
        let mut killed = false;
        while let Some(event) = client.next_event().await {
            match event {
                // The turn is open: the shim has streamed and now parks the prompt.
                Event::AgentMessageChunk { ref text, .. } if text == "done" && !killed => {
                    let pid = std::fs::read_to_string(record_dir.join("pid")).unwrap();
                    signal("-KILL", pid.trim());
                    killed = true;
                }
                Event::IntegrationLost { reason } => return Some(reason),
                Event::AgentStartupError { message, .. } => {
                    panic!("a proxy exit is not a startup error: {message}")
                }
                _ => {}
            }
        }
        None
    })
    .await
    .expect("the proxy's death is reported");
    assert_eq!(lost, Some(IntegrationLostReason::ProxyExited));
}
