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

/// Install inside the session's running sandbox container via `<runtime> exec`.
async fn install_in_container(
    session_id: &str,
    package: &str,
) -> Result<std::process::Output, Response> {
    use crate::containers::DockerContainer;

    let sid = session_id.to_string();
    let running = tokio::task::spawn_blocking(move || {
        DockerContainer::from_session_id(&sid)
            .is_running()
            .unwrap_or(false)
    })
    .await
    .unwrap_or(false);
    if !running {
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

/// `POST /api/sessions/{id}/acp/install-agent`: install the session's adapter
/// where it runs (host, or inside a sandboxed session's container). Opt-in via
/// `acp.allow_agent_install`; the package comes from a static server-side
/// table, never from client input.
pub async fn install_agent(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Some(resp) = read_only_block(&state) {
        return resp;
    }
    if let Some(resp) = cityhall_block(&state) {
        return resp;
    }
    if !crate::session::Config::load_or_warn()
        .acp
        .allow_agent_install
    {
        return api_error(
            StatusCode::FORBIDDEN,
            "install_disabled",
            "Installing agents from the web is off. Enable acp.allow_agent_install (Settings, local only).",
        );
    }
    let Some(instance) = find_instance(&state, &id).await else {
        return session_not_found();
    };

    let agent = pick_agent(&state, &instance, instance.agent_name.as_deref()).await;
    let binary = match state.acp_supervisor.resolve_agent(&agent).await {
        Ok(spec) => spec.command,
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
