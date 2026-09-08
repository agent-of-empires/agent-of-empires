//! Test fixtures shared by more than one submodule's tests.

use agent_client_protocol::schema::v1::SessionUpdate;

use crate::acp::agent_registry::AgentSpec;

use super::spawn::SpawnConfig;

pub(super) fn text_chunk(text: &str, id: Option<&str>) -> SessionUpdate {
    use agent_client_protocol::schema::v1::{ContentBlock, ContentChunk, TextContent};
    let mut chunk = ContentChunk::new(ContentBlock::Text(TextContent::new(text)));
    if let Some(id) = id {
        chunk = chunk.message_id(id);
    }
    SessionUpdate::AgentMessageChunk(chunk)
}

/// Build a minimal host (non-sandboxed) `SpawnConfig` for env tests.
pub(super) fn env_test_spawn_config(cwd: std::path::PathBuf) -> SpawnConfig {
    SpawnConfig {
        wrapper_substitution: None,
        agent_key: "claude".into(),
        tool: "claude".into(),
        spec: AgentSpec {
            command: "claude-agent-acp".into(),
            args: vec![],
            description: "test".into(),
            env_allowlist: None,
        },
        cwd,
        additional_dirs: vec![],
        provider_env: vec![],
        host_environment: vec![],
        default_effort: None,
        default_mode: None,
        socket_path: None,
        stored_acp_session_id: None,
        fork_from: None,
        seed_history_replay: false,
        artifact_dir: None,
        sandbox_info: None,
        source_profile: None,
        mcp_servers: Vec::new(),
    }
}

/// Spawn config for a `/bin/sh` fixture script.
///
/// The script is handed to `sh` as an argument rather than exec'd
/// directly. `execve` on a file any process still holds open for writing
/// fails with `ETXTBSY`, and a concurrent spawn elsewhere in the test
/// binary can fork between the fixture writer's `open` and its `close`,
/// leaving the child that writable descriptor until it execs. `sh` only
/// ever opens the script for reading, which removes the window instead of
/// retrying past it (#3790). Fixture writers therefore need no exec bit.
#[cfg(unix)]
pub(super) fn reset_fake_spawn_config(
    script: &std::path::Path,
    cwd: &std::path::Path,
) -> SpawnConfig {
    SpawnConfig {
        wrapper_substitution: None,
        agent_key: "codex".into(),
        tool: "codex".into(),
        spec: AgentSpec {
            command: "/bin/sh".into(),
            args: vec![script.to_string_lossy().into_owned()],
            description: "scripted reset fake".into(),
            env_allowlist: None,
        },
        cwd: cwd.to_path_buf(),
        additional_dirs: vec![],
        provider_env: vec![],
        host_environment: vec![],
        default_effort: None,
        default_mode: None,
        socket_path: None,
        stored_acp_session_id: None,
        fork_from: None,
        seed_history_replay: false,
        artifact_dir: None,
        sandbox_info: None,
        source_profile: None,
        mcp_servers: Vec::new(),
    }
}
