//! Import through ACP `session/list`: an id the list returned loads via `session/load` in its
//! listed cwd, and the adapter's history replay reaches the client because an import seeds an
//! empty event store.

use std::time::Duration;

use agent_of_empires::acp::acp_client::{list_native_sessions, AcpClient, SpawnConfig};
use agent_of_empires::acp::agent_registry::AgentSpec;
use agent_of_empires::acp::state::{AcpSessionId, Event};
use agent_of_empires::session::import::Owned;

use crate::common::{shim_node, shim_path, shim_ready};

fn spawn_config(env: Vec<(String, String)>) -> SpawnConfig {
    SpawnConfig {
        wrapper_substitution: None,
        agent_key: "pi".into(),
        tool: "pi".into(),
        spec: AgentSpec {
            command: shim_node()
                .expect("shim prerequisite")
                .to_string_lossy()
                .into_owned(),
            args: vec![shim_path().to_string_lossy().into_owned()],
            description: "import shim".into(),
            env_allowlist: None,
        },
        cwd: std::env::temp_dir(),
        additional_dirs: vec![],
        provider_env: env,
        provider_routing: vec![],
        host_environment: vec![],
        default_effort_explicit: false,
        default_effort: None,
        default_mode: None,
        default_model: None,
        socket_path: None,
        stored_acp_session_id: None,
        fork_from: None,
        seed_history_replay: false,
        generation: 0,
        artifact_dir: None,
        sandbox_info: None,
        source_profile: None,
        managed_profile: None,
        execution_admission: None,
        mcp_servers: Vec::new(),
        claude_store_pin: None,
        base_host_environment: vec![],
    }
}

#[tokio::test]
#[serial_test::parallel]
async fn listed_session_imports_in_its_listed_cwd_and_replays_history() {
    if let Err(reason) = shim_ready() {
        eprintln!("skipping: {reason}");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let load_record = tmp.path().join("load.log");
    let listing = serde_json::json!([{
        "sessionId": "native-1",
        "cwd": project,
        "title": "pi work",
        "updatedAt": "2026-10-01T09:00:00Z",
    }]);
    let env = vec![
        ("SHIM_LOAD_SESSION".to_string(), "1".to_string()),
        ("SHIM_LIST_SESSIONS".to_string(), listing.to_string()),
        (
            "SHIM_LOAD_RECORD_FILE".to_string(),
            load_record.to_string_lossy().into_owned(),
        ),
        ("SHIM_LOAD_REPLAY".to_string(), "from history".to_string()),
    ];

    let list = list_native_sessions(spawn_config(env.clone()), &Owned::default())
        .await
        .expect("list shim sessions");
    assert!(!list.truncated);
    let [listed] = list.sessions.as_slice() else {
        panic!("one listed session, got {:?}", list.sessions);
    };
    assert_eq!(listed.session_id, "native-1");
    assert!(listed.cwd_exists);
    assert!(
        !load_record.exists(),
        "listing must not load or create a session"
    );

    let mut config = spawn_config(env);
    config.cwd = listed.cwd.clone().into();
    config.stored_acp_session_id = Some(listed.session_id.clone());
    config.seed_history_replay = true;
    let mut client = AcpClient::spawn(config, AcpSessionId("import".into()))
        .await
        .expect("spawn import");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let event = tokio::time::timeout_at(deadline, client.next_event())
            .await
            .expect("history replay before deadline")
            .expect("event stream open");
        if matches!(&event, Event::AgentMessageChunk { text } if text == "from history") {
            break;
        }
    }
    assert_eq!(
        std::fs::read_to_string(&load_record).unwrap(),
        format!("native-1 {}\n", project.display())
    );
    let _ = client.shutdown().await;
}
