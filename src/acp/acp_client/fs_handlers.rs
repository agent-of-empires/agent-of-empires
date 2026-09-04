//! `fs/read_text_file` and `fs/write_text_file`, which the agent delegates
//! to aoe because aoe owns the disk.

use agent_client_protocol::schema::v1::{
    ReadTextFileRequest, ReadTextFileResponse, WriteTextFileRequest, WriteTextFileResponse,
};
use agent_client_protocol::Responder;
use std::sync::Arc;
use tracing::{trace, warn};

use crate::acp::fs_handler;

use super::SessionResources;

/// Issue #1147: monotonic ns-since-process-start, used as a thin
/// correlation token in the structured view ACP tool-dispatch trace. Wall-clock
/// fields like `chrono::Utc::now()` jitter under NTP slew and are too
/// coarse to detect interleaved entry/exit between concurrent handlers;
/// `Instant` is monotonic and ns-resolved on every supported platform.
/// Cast to `u64` because `Instant::elapsed()` returns `Duration` whose
/// `as_nanos()` is `u128`, which `tracing` formats less compactly. A
/// `u64` of ns gives ~584 years of headroom, which is plenty.
pub(super) fn enter_timestamp_ns() -> u64 {
    use std::sync::OnceLock;
    static EPOCH: OnceLock<std::time::Instant> = OnceLock::new();
    let epoch = EPOCH.get_or_init(std::time::Instant::now);
    epoch.elapsed().as_nanos() as u64
}

/// Run a synchronous `fs_handler` operation on the blocking pool and
/// flatten the join + handler result into a single `FsError`. Centralizes
/// the panic / cancellation observability so future fs offload sites
/// stay consistent (the offload series spans seven PRs).
pub(super) async fn spawn_blocking_fs<F, T>(
    handler: &'static str,
    f: F,
) -> Result<T, fs_handler::FsError>
where
    F: FnOnce() -> Result<T, fs_handler::FsError> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(inner) => inner,
        Err(e) => {
            warn!(
                target: "acp.protocol",
                handler,
                panic = e.is_panic(),
                cancelled = e.is_cancelled(),
                error = %e,
                "fs blocking task join failed"
            );
            Err(fs_handler::FsError::Io(std::io::Error::other(format!(
                "fs {handler} join: {e}"
            ))))
        }
    }
}

pub(super) async fn handle_read_text_file(
    request: ReadTextFileRequest,
    responder: Responder<ReadTextFileResponse>,
    res: SessionResources,
) -> agent_client_protocol::Result<()> {
    // Issue #1147: parallel-tool-call diagnostics. The `enter_ns` value is a
    // monotonic ns-since-process-start counter; if the model dispatches N
    // tool calls in parallel, the entries should interleave (close `enter_ns`
    // values across handlers) rather than strictly increasing per-handler.
    let enter_ns = enter_timestamp_ns();
    trace!(
        target: "acp.protocol.tool_dispatch",
        handler = "read_text_file",
        path = %request.path.display(),
        enter_ns,
        "ACP request handler entered"
    );
    // Offload the synchronous file read to the blocking pool. ACP
    // `fs/read_text_file` is agent driven; a multi-MB file or a slow
    // disk would otherwise stall the runtime worker for the duration
    // of the read, blocking every other ACP handler scheduled on the
    // same worker. FsPolicy is Arc + Clone so the clone is cheap.
    let policy = Arc::clone(&res.fs_policy);
    let label = res.label.clone();
    let ReadTextFileRequest {
        path, line, limit, ..
    } = request;
    let read_outcome = spawn_blocking_fs("read", move || {
        fs_handler::handle_read(&policy, &label, &path)
    })
    .await;
    let result = match read_outcome {
        Ok(content) => {
            // Honor optional line/limit slicing for ACP semantics: 1-based.
            let sliced = if line.is_some() || limit.is_some() {
                let lines: Vec<&str> = content.lines().collect();
                let start = line.map(|l| l.saturating_sub(1) as usize).unwrap_or(0);
                let limit = limit.map(|n| n as usize).unwrap_or(usize::MAX);
                let end = start.saturating_add(limit).min(lines.len());
                if start >= lines.len() {
                    String::new()
                } else {
                    lines[start..end].join("\n")
                }
            } else {
                content
            };
            responder.respond(ReadTextFileResponse::new(sliced))
        }
        Err(e) => {
            responder.respond_with_error(agent_client_protocol::util::internal_error(e.to_string()))
        }
    };
    trace!(
        target: "acp.protocol.tool_dispatch",
        handler = "read_text_file",
        enter_ns,
        elapsed_ns = enter_timestamp_ns() - enter_ns,
        "ACP request handler exited"
    );
    result
}

pub(super) async fn handle_write_text_file(
    request: WriteTextFileRequest,
    responder: Responder<WriteTextFileResponse>,
    res: SessionResources,
) -> agent_client_protocol::Result<()> {
    let enter_ns = enter_timestamp_ns();
    trace!(
        target: "acp.protocol.tool_dispatch",
        handler = "write_text_file",
        path = %request.path.display(),
        enter_ns,
        "ACP request handler entered"
    );
    // Offload the synchronous file write to the blocking pool. ACP
    // `fs/write_text_file` is agent driven; a large content payload
    // or a slow disk would otherwise stall the runtime worker for
    // the duration of the write.
    let policy = Arc::clone(&res.fs_policy);
    let label = res.label.clone();
    let WriteTextFileRequest { path, content, .. } = request;
    let write_outcome = spawn_blocking_fs("write", move || {
        fs_handler::handle_write(&policy, &label, &path, &content)
    })
    .await;
    let result = match write_outcome {
        Ok(()) => responder.respond(WriteTextFileResponse::new()),
        Err(e) => {
            responder.respond_with_error(agent_client_protocol::util::internal_error(e.to_string()))
        }
    };
    trace!(
        target: "acp.protocol.tool_dispatch",
        handler = "write_text_file",
        enter_ns,
        elapsed_ns = enter_timestamp_ns() - enter_ns,
        "ACP request handler exited"
    );
    result
}
