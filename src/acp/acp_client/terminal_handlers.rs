//! `terminal/*`: create, read output from, wait on, kill, and release the
//! terminals an agent asks aoe to run.

use agent_client_protocol::schema::v1::{
    CreateTerminalRequest, CreateTerminalResponse, KillTerminalRequest, KillTerminalResponse,
    ReleaseTerminalRequest, ReleaseTerminalResponse, TerminalId, TerminalOutputRequest,
    TerminalOutputResponse, WaitForTerminalExitRequest, WaitForTerminalExitResponse,
};
use agent_client_protocol::Responder;
use tracing::trace;

use super::fs_handlers::enter_timestamp_ns;
use super::SessionResources;

pub(super) async fn handle_create_terminal(
    request: CreateTerminalRequest,
    responder: Responder<CreateTerminalResponse>,
    res: SessionResources,
) -> agent_client_protocol::Result<()> {
    let enter_ns = enter_timestamp_ns();
    trace!(
        target: "acp.protocol.tool_dispatch",
        handler = "create_terminal",
        command = %request.command,
        argc = request.args.len(),
        enter_ns,
        "ACP request handler entered"
    );
    let cwd = request.cwd.clone().unwrap_or_else(|| res.cwd.clone());
    // Sandbox the cwd: must be inside session roots.
    if let Err(e) = res.fs_policy.resolve_inside(&cwd) {
        let r = responder.respond_with_error(agent_client_protocol::util::internal_error(format!(
            "terminal cwd outside session roots: {e}"
        )));
        trace!(
            target: "acp.protocol.tool_dispatch",
            handler = "create_terminal",
            enter_ns,
            elapsed_ns = enter_timestamp_ns() - enter_ns,
            outcome = "cwd_outside_roots",
            "ACP request handler exited"
        );
        return r;
    }
    let terminal_sandbox =
        res.sandbox
            .as_ref()
            .map(|s| crate::acp::terminal_handler::TerminalSandbox {
                container_name: s.container_name.clone(),
                env_entries: s.current_env_entries(),
            });
    let result = match res
        .terminals
        .create_and_run(
            &res.label,
            &request.command,
            request.args.clone(),
            cwd,
            terminal_sandbox.as_ref(),
        )
        .await
    {
        Ok(id) => responder.respond(CreateTerminalResponse::new(TerminalId::new(id))),
        Err(e) => {
            responder.respond_with_error(agent_client_protocol::util::internal_error(e.to_string()))
        }
    };
    trace!(
        target: "acp.protocol.tool_dispatch",
        handler = "create_terminal",
        enter_ns,
        elapsed_ns = enter_timestamp_ns() - enter_ns,
        "ACP request handler exited"
    );
    result
}

pub(super) fn build_exit_status(
    exit_code: Option<i32>,
) -> agent_client_protocol::schema::v1::TerminalExitStatus {
    use agent_client_protocol::schema::v1::TerminalExitStatus;
    let cast = exit_code.and_then(|c| u32::try_from(c).ok());
    TerminalExitStatus::new().exit_code(cast)
}

pub(super) async fn handle_terminal_output(
    request: TerminalOutputRequest,
    responder: Responder<TerminalOutputResponse>,
    res: SessionResources,
) -> agent_client_protocol::Result<()> {
    let enter_ns = enter_timestamp_ns();
    trace!(
        target: "acp.protocol.tool_dispatch",
        handler = "terminal_output",
        terminal_id = %request.terminal_id.0,
        enter_ns,
        "ACP request handler entered"
    );
    let result = match res.terminals.output(request.terminal_id.0.as_ref()).await {
        Ok(out) => {
            let combined = format!("{}{}", out.stdout, out.stderr);
            responder.respond(
                TerminalOutputResponse::new(combined, false)
                    .exit_status(build_exit_status(out.exit_code)),
            )
        }
        Err(e) => {
            responder.respond_with_error(agent_client_protocol::util::internal_error(e.to_string()))
        }
    };
    trace!(
        target: "acp.protocol.tool_dispatch",
        handler = "terminal_output",
        enter_ns,
        elapsed_ns = enter_timestamp_ns() - enter_ns,
        "ACP request handler exited"
    );
    result
}

pub(super) async fn handle_wait_for_terminal_exit(
    request: WaitForTerminalExitRequest,
    responder: Responder<WaitForTerminalExitResponse>,
    res: SessionResources,
) -> agent_client_protocol::Result<()> {
    let enter_ns = enter_timestamp_ns();
    trace!(
        target: "acp.protocol.tool_dispatch",
        handler = "wait_for_terminal_exit",
        terminal_id = %request.terminal_id.0,
        enter_ns,
        "ACP request handler entered"
    );
    // For our one-shot terminal model, the command has already finished by
    // the time `create_and_run` returns. So `output()` immediately yields
    // the captured exit status.
    let result = match res.terminals.output(request.terminal_id.0.as_ref()).await {
        Ok(out) => responder.respond(WaitForTerminalExitResponse::new(build_exit_status(
            out.exit_code,
        ))),
        Err(e) => {
            responder.respond_with_error(agent_client_protocol::util::internal_error(e.to_string()))
        }
    };
    trace!(
        target: "acp.protocol.tool_dispatch",
        handler = "wait_for_terminal_exit",
        enter_ns,
        elapsed_ns = enter_timestamp_ns() - enter_ns,
        "ACP request handler exited"
    );
    result
}

pub(super) async fn handle_kill_terminal(
    request: KillTerminalRequest,
    responder: Responder<KillTerminalResponse>,
    _res: SessionResources,
) -> agent_client_protocol::Result<()> {
    let enter_ns = enter_timestamp_ns();
    trace!(
        target: "acp.protocol.tool_dispatch",
        handler = "kill_terminal",
        terminal_id = %request.terminal_id.0,
        enter_ns,
        "ACP request handler entered"
    );
    // One-shot terminals are already finished; kill is a no-op.
    let result = responder.respond(KillTerminalResponse::new());
    trace!(
        target: "acp.protocol.tool_dispatch",
        handler = "kill_terminal",
        enter_ns,
        elapsed_ns = enter_timestamp_ns() - enter_ns,
        "ACP request handler exited"
    );
    result
}

pub(super) async fn handle_release_terminal(
    request: ReleaseTerminalRequest,
    responder: Responder<ReleaseTerminalResponse>,
    res: SessionResources,
) -> agent_client_protocol::Result<()> {
    let enter_ns = enter_timestamp_ns();
    trace!(
        target: "acp.protocol.tool_dispatch",
        handler = "release_terminal",
        terminal_id = %request.terminal_id.0,
        enter_ns,
        "ACP request handler entered"
    );
    let result = match res.terminals.release(request.terminal_id.0.as_ref()).await {
        Ok(()) => responder.respond(ReleaseTerminalResponse::new()),
        Err(e) => {
            responder.respond_with_error(agent_client_protocol::util::internal_error(e.to_string()))
        }
    };
    trace!(
        target: "acp.protocol.tool_dispatch",
        handler = "release_terminal",
        enter_ns,
        elapsed_ns = enter_timestamp_ns() - enter_ns,
        "ACP request handler exited"
    );
    result
}
