//! `terminal/*`: create, read output from, wait on, kill, and release the
//! terminals an agent asks aoe to run.

use agent_client_protocol::schema::v1::{
    CreateTerminalRequest, CreateTerminalResponse, KillTerminalRequest, KillTerminalResponse,
    ReleaseTerminalRequest, ReleaseTerminalResponse, TerminalId, TerminalOutputRequest,
    TerminalOutputResponse, WaitForTerminalExitRequest, WaitForTerminalExitResponse,
};

use std::sync::Arc;

use super::connection::ConnectionState;
use crate::acp::establishment::NativeRequestContext;
use crate::acp::terminal_handler::{CapturedTerminal, TerminalError};
use tracing::trace;

use super::fs_handlers::enter_timestamp_ns;
use super::SessionResources;

pub(super) async fn handle_create_terminal(
    request: CreateTerminalRequest,
    res: SessionResources,
    context: Option<Arc<NativeRequestContext>>,
    state: Arc<ConnectionState>,
) -> Result<CreateTerminalResponse, agent_client_protocol::Error> {
    let enter_ns = enter_timestamp_ns();
    trace!(
        target: "acp.protocol.tool_dispatch",
        handler = "create_terminal",
        command = %request.command,
        argc = request.args.len(),
        enter_ns,
        "ACP request handler entered"
    );
    let cwd = request.cwd.unwrap_or_else(|| res.cwd.clone());
    let terminal_sandbox = {
        require_native_context(context.as_deref(), &state)?;
        let _current = context
            .as_deref()
            .map(NativeRequestContext::try_original_io_guard)
            .transpose()
            .map_err(|error| agent_client_protocol::util::internal_error(error.to_string()))?;
        // Policy resolution is synchronous; no guard survives the child await.
        if let Err(e) = res.fs_policy.resolve_inside(&cwd) {
            let r = Err(agent_client_protocol::util::internal_error(format!(
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
        res.sandbox
            .as_ref()
            .map(|s| crate::acp::terminal_handler::TerminalSandbox {
                container_name: s.container_name.clone(),
                env_entries: s.current_env_entries(),
            })
    };
    let result = match res
        .terminals
        .create_and_run(
            &res.label,
            (&request.command, request.args),
            cwd,
            terminal_sandbox.as_ref(),
            context,
            move |command| {
                state
                    .admit(|| command.spawn())
                    .map_err(|error| TerminalError::Authority(error.to_string()))?
                    .map_err(TerminalError::Io)
            },
        )
        .await
    {
        Ok(id) => Ok(CreateTerminalResponse::new(TerminalId::new(id))),
        Err(e) => Err(agent_client_protocol::util::internal_error(e.to_string())),
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

fn require_native_context(
    context: Option<&NativeRequestContext>,
    state: &ConnectionState,
) -> agent_client_protocol::Result<()> {
    if context.is_none() && state.control.get().is_some() {
        return Err(agent_client_protocol::util::internal_error(
            "native terminal callback scope missing",
        ));
    }
    Ok(())
}

fn capture_terminal(
    res: &SessionResources,
    id: &str,
    context: Option<&NativeRequestContext>,
    state: &ConnectionState,
) -> agent_client_protocol::Result<CapturedTerminal> {
    require_native_context(context, state)?;
    res.terminals
        .capture(id, context)
        .map_err(|error| agent_client_protocol::util::internal_error(error.to_string()))
}

pub(super) fn handle_terminal_output(
    request: TerminalOutputRequest,
    res: SessionResources,
    context: Option<Arc<NativeRequestContext>>,
    state: Arc<ConnectionState>,
) -> impl std::future::Future<Output = agent_client_protocol::Result<TerminalOutputResponse>> + Send
{
    let terminal = capture_terminal(
        &res,
        request.terminal_id.0.as_ref(),
        context.as_deref(),
        &state,
    );
    async move {
        terminal?
            .with_output(|output| {
                TerminalOutputResponse::new(format!("{}{}", output.stdout, output.stderr), false)
                    .exit_status(build_exit_status(output.exit_code))
            })
            .map_err(|error| agent_client_protocol::util::internal_error(error.to_string()))
    }
}

pub(super) fn handle_wait_for_terminal_exit(
    request: WaitForTerminalExitRequest,
    res: SessionResources,
    context: Option<Arc<NativeRequestContext>>,
    state: Arc<ConnectionState>,
) -> impl std::future::Future<Output = agent_client_protocol::Result<WaitForTerminalExitResponse>> + Send
{
    let terminal = capture_terminal(
        &res,
        request.terminal_id.0.as_ref(),
        context.as_deref(),
        &state,
    );
    async move {
        // One-shot creation already awaited exit; read this original allocation.
        terminal?
            .with_output(|output| {
                WaitForTerminalExitResponse::new(build_exit_status(output.exit_code))
            })
            .map_err(|error| agent_client_protocol::util::internal_error(error.to_string()))
    }
}

pub(super) fn handle_kill_terminal(
    request: KillTerminalRequest,
    res: SessionResources,
    context: Option<Arc<NativeRequestContext>>,
    state: Arc<ConnectionState>,
) -> impl std::future::Future<Output = agent_client_protocol::Result<KillTerminalResponse>> + Send {
    let terminal = capture_terminal(
        &res,
        request.terminal_id.0.as_ref(),
        context.as_deref(),
        &state,
    );
    async move {
        // One-shot terminals have exited, but their original ownership is required.
        terminal?
            .with_output(|_| KillTerminalResponse::new())
            .map_err(|error| agent_client_protocol::util::internal_error(error.to_string()))
    }
}

pub(super) fn handle_release_terminal(
    request: ReleaseTerminalRequest,
    res: SessionResources,
    context: Option<Arc<NativeRequestContext>>,
    state: Arc<ConnectionState>,
) -> impl std::future::Future<Output = agent_client_protocol::Result<ReleaseTerminalResponse>> + Send
{
    let terminal = capture_terminal(
        &res,
        request.terminal_id.0.as_ref(),
        context.as_deref(),
        &state,
    );
    async move {
        res.terminals
            .release(&terminal?)
            .map_err(|error| agent_client_protocol::util::internal_error(error.to_string()))?;
        Ok(ReleaseTerminalResponse::new())
    }
}
