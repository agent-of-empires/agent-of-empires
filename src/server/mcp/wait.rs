//! Tools that let an orchestrating agent wait on a session and settle its approvals, so a
//! worker that is not in yolo mode does not hang on a question nobody sees.

use std::time::Duration;

use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::CallToolResult;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::{self, error::RecvError};

use super::control::{not_found, transcript_page};
use super::{read_only_refusal, tool_error, McpServer};
use crate::acp::acp_client::AcpError;
use crate::acp::approvals::{ApprovalDecision, ApprovalOption, Nonce};
use crate::acp::supervisor::SupervisorError;
use crate::server::api::sessions::settleable_approvals;
use crate::server::push::StatusChange;
use crate::session::{Status, View};

const DEFAULT_WAIT_SECS: u64 = 60;
const MAX_WAIT_SECS: u64 = 300;

/// A session status in its HTTP API spelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
enum WireStatus {
    Running,
    Waiting,
    Idle,
    Unknown,
    Stopped,
    Error,
    Starting,
    Deleting,
    Creating,
}

impl From<Status> for WireStatus {
    fn from(status: Status) -> Self {
        match status {
            Status::Running => Self::Running,
            Status::Waiting => Self::Waiting,
            Status::Idle => Self::Idle,
            Status::Unknown => Self::Unknown,
            Status::Stopped => Self::Stopped,
            Status::Error => Self::Error,
            Status::Starting => Self::Starting,
            Status::Deleting => Self::Deleting,
            Status::Creating => Self::Creating,
        }
    }
}

fn default_states() -> Vec<WireStatus> {
    vec![
        WireStatus::Idle,
        WireStatus::Waiting,
        WireStatus::Stopped,
        WireStatus::Error,
    ]
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct WaitForParams {
    session_id: String,
    /// Statuses to wait for. Defaults to `Idle`, `Waiting`, `Stopped` and `Error`.
    #[serde(default = "default_states")]
    states: Vec<WireStatus>,
    /// Seconds to wait, at most 300. Defaults to 60.
    timeout_s: Option<u64>,
    /// Structured view: the `seq` that `send` returned. The result carries the transcript
    /// after it, and an `Idle` status counts only once the turn after it has ended.
    after_seq: Option<u64>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct WaitResult {
    status: WireStatus,
    /// The deadline passed before the status matched; `status` is the current one.
    timed_out: bool,
    /// `after_seq` only: the transcript after it, as `read_output` renders it.
    #[serde(skip_serializing_if = "Option::is_none")]
    messages: Option<String>,
    /// `after_seq` only: pass as `read_output`'s `since` to read on.
    #[serde(skip_serializing_if = "Option::is_none")]
    next_since: Option<u64>,
    /// `after_seq` only: more transcript follows `next_since` already.
    #[serde(skip_serializing_if = "Option::is_none")]
    has_more: Option<bool>,
    pending_approvals: Vec<PendingApproval>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct PendingApproval {
    /// Pass to `approve`.
    nonce: String,
    tool_name: String,
    /// The path or command the tool call targets, else the start of its arguments.
    summary: String,
    /// Flagged as destructive, such as `rm -rf` or `git push --force`.
    destructive: bool,
    /// The options are a list of answers, so `approve` must name one with `option`.
    choice: bool,
    options: Vec<ApprovalOption>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct Approvals {
    approvals: Vec<PendingApproval>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct SessionParams {
    session_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum Decision {
    /// Allow this call once.
    Allow,
    /// Allow it and add a permission rule for calls like it.
    Always,
    Deny,
    /// Pick the option named by `option_id`.
    Option,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct ApproveParams {
    session_id: String,
    nonce: String,
    decision: Decision,
    /// With `option`: the `option_id` of one of the approval's options.
    option_id: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct Approved {
    approved: bool,
}

enum Woke {
    Changed(Status),
    /// Changes were dropped, so the store is read again.
    Lagged,
    Deadline,
    Shutdown,
}

/// Characters of raw arguments a summary falls back to when no target can be named.
const SUMMARY_CHARS: usize = 200;

#[rmcp::tool_router(router = wait_router, vis = "pub(super)")]
impl McpServer {
    #[rmcp::tool(
        description = "Block until a session's status is one of `states` or `timeout_s` passes \
            (a timeout is a normal result). Returns at once when the status already matches. \
            With `after_seq` from `send`, also returns the transcript since that prompt, and waits \
            for its turn to end. Lists the session's pending approvals, to settle with `approve`."
    )]
    async fn wait_for(
        &self,
        Parameters(params): Parameters<WaitForParams>,
    ) -> Result<Json<WaitResult>, CallToolResult> {
        let id = params.session_id;
        let deadline = tokio::time::Instant::now() + wait_timeout(params.timeout_s);
        // Subscribe before the first read so no change between the two is missed.
        let mut changes = self.state.status_tx.subscribe();
        let (mut current, view) = self.session_of(&id).await.ok_or_else(|| not_found(&id))?;
        // A terminal session has no event seqs, so there is no turn to wait for.
        let after_seq = params.after_seq.filter(|_| view == View::Structured);
        let timed_out = loop {
            if current == Status::Deleting {
                return Err(not_found(&id));
            }
            if self.settled(&id, current, &params.states, after_seq) {
                break false;
            }
            current = match self.next_change(&id, &mut changes, deadline).await {
                Woke::Changed(status) => status,
                Woke::Lagged => self.status_of(&id).await.ok_or_else(|| not_found(&id))?,
                Woke::Deadline => break true,
                Woke::Shutdown => {
                    return Err(tool_error("busy", "The daemon is shutting down"));
                }
            };
        };
        if timed_out {
            current = self.status_of(&id).await.ok_or_else(|| not_found(&id))?;
            if current == Status::Deleting {
                return Err(not_found(&id));
            }
        }
        let page = after_seq.map(|since| transcript_page(&self.state, &id, since));
        Ok(Json(WaitResult {
            status: current.into(),
            timed_out,
            next_since: page.as_ref().map(|(_, next, _)| *next),
            has_more: page.as_ref().map(|(_, _, more)| *more),
            messages: page.map(|(text, _, _)| text),
            pending_approvals: self.pending_approvals(&id, view).await,
        }))
    }

    #[rmcp::tool(
        description = "A structured session's pending approvals, to settle with `approve`. \
            Always empty for a terminal session."
    )]
    async fn list_approvals(
        &self,
        Parameters(params): Parameters<SessionParams>,
    ) -> Result<Json<Approvals>, CallToolResult> {
        let id = params.session_id;
        let view = self.view_of(&id).await.ok_or_else(|| not_found(&id))?;
        Ok(Json(Approvals {
            approvals: self.pending_approvals(&id, view).await,
        }))
    }

    #[rmcp::tool(
        description = "Settle a structured session's pending approval by its nonce: `allow` once, \
            `always` (allow and remember), `deny`, or `option` with an `option_id`. An approval \
            whose `choice` is true takes only `option`."
    )]
    async fn approve(
        &self,
        Parameters(params): Parameters<ApproveParams>,
    ) -> Result<Json<Approved>, CallToolResult> {
        if let Some(refusal) = read_only_refusal(&self.state) {
            return Err(refusal);
        }
        let id = params.session_id;
        match self.view_of(&id).await {
            None => return Err(not_found(&id)),
            Some(View::Terminal) => {
                return Err(tool_error(
                    "invalid_request",
                    "A terminal session has no approvals to settle",
                ))
            }
            Some(View::Structured) => {}
        }
        let unknown = || {
            tool_error(
                "not_found",
                format!("No pending approval {} on session {id}", params.nonce),
            )
        };
        let approval = self
            .state
            .acp_event_store
            .pending_approval_requests(&id)
            .into_iter()
            .find(|a| a.nonce.0 == params.nonce)
            .ok_or_else(unknown)?;
        let (decision, option_id) =
            match params.decision {
                Decision::Option => {
                    let Some(option_id) = params.option_id else {
                        return Err(tool_error(
                            "invalid_request",
                            "`option` needs an `option_id`",
                        ));
                    };
                    if !approval.options.iter().any(|o| o.option_id == option_id) {
                        return Err(tool_error(
                            "invalid_request",
                            format!("The approval has no option {option_id}"),
                        ));
                    }
                    // The named option outranks the decision sent with it.
                    (ApprovalDecision::Allow, Some(option_id))
                }
                // An allow/deny answer to a list of answers would cancel it.
                _ if approval.choice => return Err(tool_error(
                    "invalid_request",
                    "This approval is a list of answers: pass `option` with one of its option ids",
                )),
                Decision::Allow => (ApprovalDecision::Allow, None),
                Decision::Always => (ApprovalDecision::AllowAlways, None),
                Decision::Deny => (ApprovalDecision::Deny, None),
            };
        self.state
            .acp_supervisor
            .resolve_permission(&id, Nonce(params.nonce.clone()), decision, option_id)
            .await
            .map_err(|e| match e {
                SupervisorError::Acp(AcpError::UnknownNonce) => unknown(),
                SupervisorError::UnknownSession(_)
                | SupervisorError::Acp(AcpError::AgentExited | AcpError::NotRunning) => {
                    tool_error("not_running", "The session's agent is not running")
                }
                other => tool_error("internal", other.to_string()),
            })?;
        Ok(Json(Approved { approved: true }))
    }
}

impl McpServer {
    async fn session_of(&self, id: &str) -> Option<(Status, View)> {
        self.state
            .instances
            .read()
            .await
            .iter()
            .find(|i| i.id == id)
            .map(|i| (i.status, i.view))
    }

    /// The next status change for `id`. Other sessions' changes are skipped here, so they cost
    /// no re-check; the deadline and shutdown win over a backlog of them.
    async fn next_change(
        &self,
        id: &str,
        changes: &mut broadcast::Receiver<StatusChange>,
        deadline: tokio::time::Instant,
    ) -> Woke {
        loop {
            tokio::select! {
                biased;
                _ = self.state.shutdown.cancelled() => return Woke::Shutdown,
                _ = tokio::time::sleep_until(deadline) => return Woke::Deadline,
                change = changes.recv() => match change {
                    Ok(change) if change.instance_id == id => return Woke::Changed(change.new),
                    Ok(_) => {}
                    Err(RecvError::Lagged(_)) => return Woke::Lagged,
                    Err(RecvError::Closed) => return Woke::Shutdown,
                },
            }
        }
    }

    async fn status_of(&self, id: &str) -> Option<Status> {
        self.state
            .instances
            .read()
            .await
            .iter()
            .find(|i| i.id == id)
            .map(|i| i.status)
    }

    /// Whether `status` ends a wait for `states`. Status lags the event log, so with `after_seq`
    /// an `Idle` read before that turn ended is the previous turn's.
    fn settled(
        &self,
        id: &str,
        status: Status,
        states: &[WireStatus],
        after_seq: Option<u64>,
    ) -> bool {
        if !states.contains(&status.into()) {
            return false;
        }
        match after_seq {
            Some(seq) if status == Status::Idle => self
                .state
                .acp_event_store
                .turn_ended_after(id, seq)
                .unwrap_or(true),
            _ => true,
        }
    }

    async fn pending_approvals(&self, id: &str, view: View) -> Vec<PendingApproval> {
        if view != View::Structured {
            return Vec::new();
        }
        let worker = self.state.acp_supervisor.worker_state(id).await;
        settleable_approvals(&self.state, id, worker)
            .into_iter()
            .map(|approval| {
                let call = approval.tool_call;
                let target =
                    crate::acp::approvals::summarize_target(&call.kind, &call.args_preview);
                let summary = if target.is_empty() {
                    call.args_preview.chars().take(SUMMARY_CHARS).collect()
                } else {
                    target
                };
                PendingApproval {
                    nonce: approval.nonce.0,
                    tool_name: call.name,
                    summary,
                    destructive: approval.destructive,
                    choice: approval.choice,
                    options: approval.options,
                }
            })
            .collect()
    }
}

fn wait_timeout(timeout_s: Option<u64>) -> Duration {
    Duration::from_secs(timeout_s.unwrap_or(DEFAULT_WAIT_SECS).min(MAX_WAIT_SECS))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_timeout_defaults_and_caps() {
        // (timeout_s, seconds waited)
        for (asked, waited) in [
            (None, 60),
            (Some(0), 0),
            (Some(30), 30),
            (Some(10_000), 300),
        ] {
            assert_eq!(
                wait_timeout(asked),
                Duration::from_secs(waited),
                "{asked:?}"
            );
        }
    }

    #[test]
    fn statuses_use_the_http_spelling() {
        for status in [
            Status::Running,
            Status::Waiting,
            Status::Idle,
            Status::Unknown,
            Status::Stopped,
            Status::Error,
            Status::Starting,
            Status::Deleting,
            Status::Creating,
        ] {
            assert_eq!(
                serde_json::to_value(WireStatus::from(status)).unwrap(),
                status.wire_str()
            );
        }
    }
}
