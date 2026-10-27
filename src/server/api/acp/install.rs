//! Server-run `npm install -g` for a session's ACP adapter (#2109).

use serde::Serialize;

use crate::server::api::{api_error, find_instance};

use super::*;

/// Serializes installs process-wide: `npm install -g` shares one global prefix.
static INSTALL_LOCK: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

/// Per-stream cap on npm output echoed back; lifecycle scripts can be verbose.
const MAX_INSTALL_LOG_BYTES: usize = 64 * 1024;

const INSTALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

fn truncate_install_log(raw: &[u8]) -> String {
    let text = String::from_utf8_lossy(raw);
    if text.len() <= MAX_INSTALL_LOG_BYTES {
        return text.into_owned();
    }
    let mut cut = MAX_INSTALL_LOG_BYTES;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!(
        "{}\n... [truncated, output exceeded {MAX_INSTALL_LOG_BYTES} bytes]",
        &text[..cut]
    )
}

/// On `success` the client respawns the session itself via `/acp/spawn`.
#[derive(Serialize)]
pub struct InstallAgentResponse {
    pub session_id: String,
    pub package: String,
    pub success: bool,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    /// Other sessions parked on the same adapter that were queued to respawn.
    pub recovered_sessions: usize,
}

/// Run a bounded install command; `kill_on_drop` reaps it on timeout.
async fn run_install(
    mut command: tokio::process::Command,
    start_error: (&str, String),
    timeout_message: &str,
) -> Result<std::process::Output, Response> {
    match tokio::time::timeout(INSTALL_TIMEOUT, command.kill_on_drop(true).output()).await {
        Ok(Ok(o)) => Ok(o),
        Ok(Err(e)) => Err(api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            start_error.0,
            format!("{}: {e}", start_error.1),
        )),
        Err(_) => Err(api_error(
            StatusCode::GATEWAY_TIMEOUT,
            "install_timeout",
            timeout_message,
        )),
    }
}

async fn install_on_host(package: &str) -> Result<std::process::Output, Response> {
    let Ok(npm) = which::which("npm") else {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "npm_missing",
            "`npm` is not on the daemon's PATH. Start `aoe serve` from a shell where `which npm` resolves.",
        ));
    };
    let mut command = tokio::process::Command::new(&npm);
    command.arg("install").arg("-g").arg(package);
    run_install(
        command,
        (
            "npm_start_failed",
            "npm install failed to start".to_string(),
        ),
        "`npm install -g` did not finish within 180s.",
    )
    .await
}

/// Whether the session's sandbox container is running.
async fn container_running(session_id: &str) -> bool {
    let sid = session_id.to_string();
    tokio::task::spawn_blocking(move || {
        crate::containers::DockerContainer::from_session_id(&sid)
            .is_running()
            .unwrap_or(false)
    })
    .await
    .unwrap_or(false)
}

/// Install inside the session's running sandbox container via `<runtime> exec`.
async fn install_in_container(
    session_id: &str,
    package: &str,
) -> Result<std::process::Output, Response> {
    use crate::containers::DockerContainer;

    if !container_running(session_id).await {
        return Err(api_error(
            StatusCode::CONFLICT,
            "container_not_running",
            "The session's sandbox container is not running. Open the session (which starts its container) and try again.",
        ));
    }

    let runtime_bin = crate::containers::runtime_binary();
    let mut command = tokio::process::Command::new(runtime_bin);
    command
        .arg("exec")
        .arg(DockerContainer::generate_name(session_id))
        .args(["npm", "install", "-g"])
        .arg(package);
    run_install(
        command,
        (
            "exec_start_failed",
            format!("`{runtime_bin} exec` failed to start"),
        ),
        "`npm install -g` in the sandbox did not finish within 180s.",
    )
    .await
}

/// The agent a session's Structured view spawns as, and its adapter binary.
async fn session_adapter(
    state: &AppState,
    instance: &crate::session::Instance,
) -> (String, Result<String, SupervisorError>) {
    let agent = pick_agent(state, instance, instance.agent_name.as_deref()).await;
    let binary = state
        .acp_supervisor
        .resolve_agent(&agent)
        .await
        .map(|spec| spec.command);
    (agent, binary)
}

/// How to install a session's adapter, for the startup error banner. Describes
/// the adapter `install-agent` installs, which for a custom `agent_acp_cmd`
/// agent is none.
#[derive(Serialize)]
pub struct AgentInstallInfo {
    pub agent: String,
    pub display_name: String,
    pub install_command: Option<&'static str>,
    pub npm_installable: bool,
    /// Whether `install-agent` accepts requests for this Session.
    pub install_allowed: bool,
    pub sandboxed: bool,
}

/// `GET /api/sessions/{id}/acp/install-info`
pub async fn agent_install_info(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let Some(instance) = find_instance(&state, &id).await else {
        return session_not_found();
    };
    let (agent, binary) = session_adapter(&state, &instance).await;
    let binary = binary.ok();
    Json(AgentInstallInfo {
        display_name: crate::acp::display_name_for(&agent)
            .unwrap_or(&agent)
            .to_string(),
        install_command: binary
            .as_deref()
            .and_then(crate::acp::install_hints::install_hint_for),
        npm_installable: binary
            .as_deref()
            .and_then(crate::acp::install_hints::npm_package_for)
            .is_some(),
        install_allowed: install_gate(&state).is_ok()
            && (!instance.is_sandboxed() || container_running(&id).await),
        sandboxed: instance.is_sandboxed(),
        agent,
    })
    .into_response()
}

/// Session-independent refusals of `install-agent`. `install-info` applies the
/// same gate plus the container check, so the banner never offers a refused install.
fn install_gate(state: &AppState) -> Result<(), Response> {
    if let Some(resp) = read_only_block(state).or_else(|| cityhall_block(state)) {
        return Err(resp);
    }
    if !crate::session::Config::load_or_warn()
        .acp
        .allow_agent_install
    {
        return Err(api_error(
            StatusCode::FORBIDDEN,
            "install_disabled",
            "Installing agents from the web is off. Enable acp.allow_agent_install (Settings, local only).",
        ));
    }
    Ok(())
}

/// `POST /api/sessions/{id}/acp/install-agent`: install the session's adapter
/// where it runs (host, or inside a sandboxed session's container). Opt-in via
/// `acp.allow_agent_install`; the package comes from a static server-side
/// table, never from client input.
pub async fn install_agent(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Err(resp) = install_gate(&state) {
        return resp;
    }
    let Some(instance) = find_instance(&state, &id).await else {
        return session_not_found();
    };

    let (agent, binary) = session_adapter(&state, &instance).await;
    let binary = match binary {
        Ok(binary) => binary,
        Err(e) => {
            return api_error(
                StatusCode::BAD_REQUEST,
                "agent_resolve_failed",
                format!("could not resolve agent `{agent}`: {e}"),
            );
        }
    };
    let Some(package) = crate::acp::install_hints::npm_package_for(&binary) else {
        let hint =
            crate::acp::install_hints::install_hint_for(&binary).unwrap_or("(see project docs)");
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "not_npm_installable",
                "message": format!("`{binary}` cannot be installed via npm from the daemon. Install it manually: {hint}"),
                "install_command": hint,
            })),
        )
            .into_response();
    };

    // The session lock also keeps a same-session spawn from running mid-install.
    let _install_guard = INSTALL_LOCK.lock().await;
    let inst_lock = state.instance_lock(&id).await;
    let _guard = inst_lock.lock().await;

    let installed = if instance.is_sandboxed() {
        install_in_container(&id, package).await
    } else {
        install_on_host(package).await
    };
    let output = match installed {
        Ok(output) => output,
        Err(resp) => return resp,
    };

    // A host install updates the shared global prefix, so other host sessions
    // parked on this binary can recover. The client respawns this one.
    let mut recovered_sessions = 0;
    if output.status.success() && !instance.is_sandboxed() {
        for other in state
            .acp_supervisor
            .incompatible_sessions_for_binary(&binary)
            .await
        {
            if other != id {
                state.acp_supervisor.request_respawn(&other);
                recovered_sessions += 1;
            }
        }
    }

    Json(InstallAgentResponse {
        session_id: id,
        package: package.to_string(),
        success: output.status.success(),
        exit_code: output.status.code(),
        stdout: truncate_install_log(&output.stdout),
        stderr: truncate_install_log(&output.stderr),
        recovered_sessions,
    })
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The banner's install command and Agent name come from the session's
    /// resolved adapter, not the session's tool or a hardcoded Claude package.
    #[tokio::test]
    #[serial_test::serial]
    async fn install_info_describes_the_sessions_adapter() {
        let _guard = crate::session::test_support::isolate_app_dir();
        let session = |id: &str, tool: &str, agent: Option<&str>, sandboxed: bool| {
            let mut inst = crate::session::Instance::new(id, "/tmp/aoe-install-info");
            inst.id = id.to_string();
            inst.tool = tool.to_string();
            inst.agent_name = agent.map(str::to_string);
            inst.sandbox_info = sandboxed.then(|| crate::session::SandboxInfo {
                provider: None,
                enabled: true,
                container_id: None,
                image: "ubuntu:latest".to_string(),
                container_name: format!("aoe-{id}"),
                extra_env: None,
                custom_instruction: None,
                before_start_env: Vec::new(),
                before_start_hook_env: Vec::new(),
                container_workdir: None,
            });
            inst
        };
        let state = crate::server::test_support::build_test_app_state(vec![
            session("claude", "claude", None, false),
            session("opencode", "opencode", None, false),
            session("override", "claude", Some("codex"), false),
            session("custom", "claude", Some("my-wrapper"), false),
            session("sandboxed", "claude", None, true),
        ]);
        let fetch = |state: Arc<AppState>, id: &'static str| async move {
            let resp = agent_install_info(State(state), Path(id.to_string()))
                .await
                .into_response();
            let status = resp.status();
            let bytes = axum::body::to_bytes(resp.into_body(), 1 << 16)
                .await
                .unwrap();
            let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
            (status, body)
        };
        let get = |id: &'static str| fetch(state.clone(), id);
        let claude_npm = "npm install -g @agentclientprotocol/claude-agent-acp@latest";
        for (id, agent, name, command, npm, sandboxed) in [
            (
                "claude",
                "claude",
                "Claude Code",
                Some(claude_npm),
                true,
                false,
            ),
            (
                "opencode",
                "opencode",
                "OpenCode",
                crate::acp::install_hints::install_hint_for("opencode"),
                false,
                false,
            ),
            (
                "override",
                "codex",
                "Codex",
                Some("npm install -g @agentclientprotocol/codex-acp@latest"),
                true,
                false,
            ),
            ("custom", "my-wrapper", "my-wrapper", None, false, false),
            (
                "sandboxed",
                "claude",
                "Claude Code",
                Some(claude_npm),
                true,
                true,
            ),
        ] {
            let (status, body) = get(id).await;
            assert_eq!(status, StatusCode::OK, "{id}: {body}");
            assert_eq!(
                body,
                serde_json::json!({
                    "agent": agent,
                    "display_name": name,
                    "install_command": command,
                    "npm_installable": npm,
                    "install_allowed": false,
                    "sandboxed": sandboxed,
                }),
                "{id}"
            );
        }

        assert_eq!(get("missing").await.0, StatusCode::NOT_FOUND);

        // `install_allowed` is false exactly where `install-agent` refuses.
        let cityhall = crate::server::test_support::build_test_app_state_cityhall(vec![session(
            "claude", "claude", None, false,
        )]);
        let read_only = crate::server::test_support::build_test_app_state_configured(
            vec![session("claude", "claude", None, false)],
            |state| state.read_only = true,
        );
        for (label, setting, state, allowed) in [
            ("setting off", false, state.clone(), false),
            ("setting on", true, state.clone(), true),
            ("CityHall", true, cityhall, false),
            ("read-only", true, read_only, false),
        ] {
            crate::session::config::update_config(|c| c.acp.allow_agent_install = setting).unwrap();
            let body = fetch(state.clone(), "claude").await.1;
            assert_eq!(body["install_allowed"], allowed, "{label}");
            if !allowed {
                let refused = install_agent(State(state), Path("claude".to_string()))
                    .await
                    .into_response();
                assert_eq!(refused.status(), StatusCode::FORBIDDEN, "{label}");
            }
        }

        // The loop leaves the setting on; a stopped container still refuses.
        let body = fetch(state.clone(), "sandboxed").await.1;
        assert_eq!(body["install_allowed"], false, "stopped container");
        let refused = install_agent(State(state), Path("sandboxed".to_string()))
            .await
            .into_response();
        assert_eq!(refused.status(), StatusCode::CONFLICT, "stopped container");
    }
}
