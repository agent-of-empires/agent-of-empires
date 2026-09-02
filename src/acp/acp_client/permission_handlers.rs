//! `session/request_permission` and elicitation requests, which aoe answers
//! from the approval policy or by asking the user.

use crate::acp::agent_profiles;
use crate::acp::approvals::{ApprovalDecision, Nonce};
use crate::acp::elicitations::{parse_elicitation, ElicitationOutcome};
use crate::acp::permissions::build_approval;
use crate::acp::state::{Event, ToolCall};
use agent_client_protocol::schema::v1::{
    CreateElicitationRequest, CreateElicitationResponse, ElicitationAction, PermissionOptionKind,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome,
};
use agent_client_protocol::Responder;
use tokio::sync::{mpsc, oneshot};
use tracing::{trace, warn};

use super::fs_handlers::enter_timestamp_ns;
use super::pending::{
    ApprovalResolutionMessage, ElicitationResolutionMessage, PendingResolver, PendingResponder,
    PendingResponders,
};
use super::tool_context::{permission_raw_input_with_context, ToolContextCache};
use super::tool_output::{preview_optional_args, tool_kind_str};

/// Translate the user's decision into the matching option_id from the
/// list the agent offered. Falls back gracefully if the agent didn't
/// offer the preferred kind.
pub(super) fn pick_option_id(
    options: &[agent_client_protocol::schema::v1::PermissionOption],
    decision: ApprovalDecision,
) -> Option<agent_client_protocol::schema::v1::PermissionOptionId> {
    let preferred_kinds = match decision {
        ApprovalDecision::Allow => &[
            PermissionOptionKind::AllowOnce,
            PermissionOptionKind::AllowAlways,
        ][..],
        ApprovalDecision::AllowAlways => &[
            PermissionOptionKind::AllowAlways,
            PermissionOptionKind::AllowOnce,
        ][..],
        ApprovalDecision::Deny => &[
            PermissionOptionKind::RejectOnce,
            PermissionOptionKind::RejectAlways,
        ][..],
        // Synthetic decision emitted by the daemon-restart rehydration
        // sweep. Has no agent option to map to (the agent never sees
        // it); the caller falls through to `RequestPermissionOutcome::
        // Cancelled` when this returns None.
        ApprovalDecision::Cancelled => &[][..],
    };
    for kind in preferred_kinds {
        if let Some(opt) = options.iter().find(|o| &o.kind == kind) {
            return Some(opt.option_id.clone());
        }
    }
    None
}

/// Close a permission-request tool card with a terminal error row when
/// the user denies (or no compatible option exists). Pairs the start
/// frame emitted in `handle_permission_request`; without it a denied tool
/// hangs on "running" until the turn ends. See #1713.
pub(super) async fn emit_permission_denied(
    event_tx: &mpsc::Sender<Event>,
    tool_call_id: &str,
    content: &str,
) {
    let _ = event_tx
        .send(Event::ToolCallCompleted {
            tool_call_id: tool_call_id.to_string(),
            is_error: true,
            content: content.to_string(),
            output: Vec::new(),
            completed_at: chrono::Utc::now(),
            async_subagent: false,
        })
        .await;
}

pub(super) async fn handle_permission_request(
    request: RequestPermissionRequest,
    responder: Responder<RequestPermissionResponse>,
    event_tx: mpsc::Sender<Event>,
    pending: PendingResponders,
    profile: &'static agent_profiles::AgentProfile,
    tool_context_cache: ToolContextCache,
) -> agent_client_protocol::Result<()> {
    let enter_ns = enter_timestamp_ns();
    let tool_call_id = request.tool_call.tool_call_id.0.to_string();
    trace!(
        target: "acp.protocol.tool_dispatch",
        handler = "permission_request",
        tool_call_id = %tool_call_id,
        enter_ns,
        "ACP request handler entered"
    );
    // Build our structured view-side approval card.
    let title = request
        .tool_call
        .fields
        .title
        .clone()
        .unwrap_or_else(|| "tool call".into());
    let cached_raw_input = tool_context_cache
        .lock()
        .expect("tool context cache mutex poisoned")
        .get(&tool_call_id);
    // Empty (not the literal "null") when neither the permission request nor a
    // previously forwarded tool update has raw_input. Gemini's confirm-required
    // tools routinely do this. See #1713. opencode sometimes sends an
    // external_directory permission reusing a tool_call_id whose earlier tool
    // update had the command, so merge that context before emitting events.
    let enriched_raw_input = permission_raw_input_with_context(
        request.tool_call.fields.raw_input.as_ref(),
        cached_raw_input.as_ref(),
    );
    let args_preview = preview_optional_args(enriched_raw_input.as_ref());
    let tool_call = ToolCall {
        id: request.tool_call.tool_call_id.0.to_string(),
        name: title,
        kind: request
            .tool_call
            .fields
            .kind
            .as_ref()
            .map(tool_kind_str)
            .unwrap_or_else(|| "other".into()),
        args_preview,
        started_at: chrono::Utc::now(),
        parent_tool_call_id: profile.parent_tool_use_id_from_meta(&request.tool_call.meta),
        memory_recall: None,
        diffs: Vec::new(),
    };
    // Gemini's confirm-required tools never send a standalone `tool_call`
    // start frame (only requestPermission, then a completion update), so
    // without this the approved tool would have no transcript card and
    // its later completion would render nothing. Emit a start frame from
    // the ToolCall we just built; the reducer dedupes tool_start by id,
    // so a later real start frame merges in place rather than doubling
    // the card. See #1713.
    let _ = event_tx
        .send(Event::ToolCallStarted {
            tool_call: tool_call.clone(),
        })
        .await;
    let approval = build_approval(tool_call);
    let nonce = approval.nonce.clone();

    let (resolve_tx, resolve_rx) = oneshot::channel::<ApprovalResolutionMessage>();
    pending.lock().await.insert(
        nonce.clone(),
        PendingResponder {
            resolver: PendingResolver::Approval(resolve_tx),
        },
    );

    if event_tx
        .send(Event::ApprovalRequested { approval })
        .await
        .is_err()
    {
        // Receiver gone: cancel.
        pending.lock().await.remove(&nonce);
        trace!(
            target: "acp.protocol.tool_dispatch",
            handler = "permission_request",
            tool_call_id = %tool_call_id,
            enter_ns,
            elapsed_ns = enter_timestamp_ns() - enter_ns,
            outcome = "receiver_gone",
            "ACP request handler exited"
        );
        return responder.respond(RequestPermissionResponse::new(
            RequestPermissionOutcome::Cancelled,
        ));
    }

    // Issue #1147: this `await` is the suspected serializer for the user-felt
    // slowness. Log the moment we begin awaiting so a wall-clock comparison
    // with later "responder.respond" emissions exposes how long each pending
    // approval blocked the agent's turn.
    let await_enter_ns = enter_timestamp_ns();
    trace!(
        target: "acp.protocol.tool_dispatch",
        handler = "permission_request",
        tool_call_id = %tool_call_id,
        enter_ns,
        await_offset_ns = await_enter_ns - enter_ns,
        "awaiting approval resolution"
    );
    // Build outcome + its label together so the exit event never re-matches on
    // a foreign `#[non_exhaustive]` enum it doesn't fully own.
    let (outcome, outcome_label): (RequestPermissionOutcome, &'static str) = match resolve_rx.await
    {
        Ok(ApprovalResolutionMessage::Decision { decision }) => {
            if let Some(option_id) = pick_option_id(&request.options, decision) {
                // Surface the resolution to UI clients via the typed event channel.
                let _ = event_tx
                    .send(Event::ApprovalResolved {
                        nonce: nonce.clone(),
                        decision,
                    })
                    .await;
                // A denied tool will not run, so the start frame emitted
                // above would otherwise hang on "running" until the turn
                // ends. Close it immediately with a terminal error row.
                // See #1713.
                if matches!(decision, ApprovalDecision::Deny) {
                    emit_permission_denied(&event_tx, &tool_call_id, "permission denied").await;
                }
                (
                    RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(option_id)),
                    "selected",
                )
            } else {
                warn!(
                    target: "acp.protocol",
                    "agent did not offer a {decision:?}-compatible option; cancelling"
                );
                // No compatible option: the agent gets Cancelled, but the
                // user still acted, so clear the approval card and close
                // the hanging start frame. See #1713.
                let _ = event_tx
                    .send(Event::ApprovalResolved {
                        nonce: nonce.clone(),
                        decision: ApprovalDecision::Cancelled,
                    })
                    .await;
                emit_permission_denied(&event_tx, &tool_call_id, "permission cancelled").await;
                (RequestPermissionOutcome::Cancelled, "cancelled")
            }
        }
        Ok(ApprovalResolutionMessage::Cancelled) | Err(_) => {
            // Cancellation (explicit cancel_permission, or the resolver
            // dropped on teardown) emits no agent completion, so close the
            // start frame and clear the approval here too. See #1713.
            let _ = event_tx
                .send(Event::ApprovalResolved {
                    nonce: nonce.clone(),
                    decision: ApprovalDecision::Cancelled,
                })
                .await;
            emit_permission_denied(&event_tx, &tool_call_id, "permission cancelled").await;
            (RequestPermissionOutcome::Cancelled, "cancelled")
        }
    };
    let exit_ns = enter_timestamp_ns();
    trace!(
        target: "acp.protocol.tool_dispatch",
        handler = "permission_request",
        tool_call_id = %tool_call_id,
        enter_ns,
        elapsed_ns = exit_ns - enter_ns,
        await_ns = exit_ns - await_enter_ns,
        outcome = outcome_label,
        "responding to permission request"
    );
    responder.respond(RequestPermissionResponse::new(outcome))
}

/// Handle an `elicitation/create` request (claude-agent-acp's
/// `AskUserQuestion`, surfaced because we advertise `elicitation.form`).
/// Mirrors `handle_permission_request`: normalize the form, park a
/// resolver under a fresh nonce, broadcast the card, await the user's
/// answer, then respond to the agent. Cancellation (resolver dropped on
/// teardown) and an unparseable schema both fall back to a graceful
/// response so the agent's turn never hangs.
pub(super) async fn handle_elicitation_request(
    request: CreateElicitationRequest,
    responder: Responder<CreateElicitationResponse>,
    event_tx: mpsc::Sender<Event>,
    pending: PendingResponders,
) -> agent_client_protocol::Result<()> {
    let nonce = Nonce::new();
    let elicitation = match parse_elicitation(nonce.clone(), &request, chrono::Utc::now()) {
        Ok(elicitation) => elicitation,
        Err(e) => {
            // A schema we can't render (URL mode, or an MCP-server form
            // with number/boolean fields). Cancel rather than Decline: the
            // question was never shown, so "user skipped" (Decline, empty
            // answer) would misrepresent it; Cancel tells the agent the
            // request could not be presented. Either way the turn does not
            // hang on a card we'll never show.
            warn!(target: "acp.protocol", "unsupported elicitation, cancelling: {e}");
            return responder.respond(CreateElicitationResponse::new(ElicitationAction::Cancel));
        }
    };

    let (resolve_tx, resolve_rx) = oneshot::channel::<ElicitationResolutionMessage>();
    pending.lock().await.insert(
        nonce.clone(),
        PendingResponder {
            resolver: PendingResolver::Elicitation {
                elicitation: Box::new(elicitation.clone()),
                resolver: resolve_tx,
            },
        },
    );

    if event_tx
        .send(Event::ElicitationRequested {
            elicitation: elicitation.clone(),
        })
        .await
        .is_err()
    {
        pending.lock().await.remove(&nonce);
        return responder.respond(CreateElicitationResponse::new(ElicitationAction::Cancel));
    }

    // Await the user's answer. `resolve_elicitation` validates server-side
    // before sending, so whatever arrives here is already a built, valid
    // response. A dropped resolver (daemon teardown, agent cancel) cancels
    // the tool call.
    let ElicitationResolutionMessage {
        response,
        outcome,
        answers,
    } = resolve_rx
        .await
        .unwrap_or_else(|_| ElicitationResolutionMessage {
            response: CreateElicitationResponse::new(ElicitationAction::Cancel),
            outcome: ElicitationOutcome::Cancelled,
            answers: Vec::new(),
        });

    let _ = event_tx
        .send(Event::ElicitationResolved {
            nonce: nonce.clone(),
            outcome,
            answers,
        })
        .await;

    responder.respond(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pick_option_id_finds_allow_once() {
        use agent_client_protocol::schema::v1::{PermissionOption, PermissionOptionId};
        let options = vec![
            PermissionOption::new(
                PermissionOptionId::new("yes"),
                "Allow this once",
                PermissionOptionKind::AllowOnce,
            ),
            PermissionOption::new(
                PermissionOptionId::new("no"),
                "Reject",
                PermissionOptionKind::RejectOnce,
            ),
        ];
        let id = pick_option_id(&options, ApprovalDecision::Allow).unwrap();
        assert_eq!(id.0.as_ref(), "yes");
    }

    #[test]
    fn pick_option_id_falls_back() {
        use agent_client_protocol::schema::v1::{PermissionOption, PermissionOptionId};
        let options = vec![PermissionOption::new(
            PermissionOptionId::new("always"),
            "Always",
            PermissionOptionKind::AllowAlways,
        )];
        // We asked for Allow (prefers AllowOnce); the agent only offered
        // AllowAlways. Falls back gracefully.
        let id = pick_option_id(&options, ApprovalDecision::Allow).unwrap();
        assert_eq!(id.0.as_ref(), "always");
    }
}
