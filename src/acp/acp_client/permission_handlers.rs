//! `session/request_permission` and elicitation requests, which aoe answers
//! from the approval policy or by asking the user.

use crate::acp::agent_profiles;
use crate::acp::approvals::{
    is_choice_list, ApprovalDecision, ApprovalOption, ApprovalOptionKind, Nonce,
};
use crate::acp::elicitations::{parse_elicitation, ElicitationOutcome};
use crate::acp::permissions::build_approval;
use crate::acp::state::{Event, ToolCall};
use agent_client_protocol::schema::v1::{
    CreateElicitationRequest, CreateElicitationResponse, ElicitationAction, PermissionOptionKind,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome,
};

use tokio::sync::{mpsc, oneshot};
use tracing::{trace, warn};

use super::fs_handlers::enter_timestamp_ns;
use super::pending::{
    ApprovalResolutionMessage, ElicitationResolutionMessage, PendingResolver, PendingResponder,
    PendingResponders,
};
use super::tool_context::{permission_raw_input_with_context, ToolContextCache};
use super::tool_output::{preview_optional_args, tool_kind_str};

/// A kind we cannot map back is dropped rather than guessed at, so no client
/// offers a button whose meaning is unknown.
pub(super) fn approval_options(
    options: &[agent_client_protocol::schema::v1::PermissionOption],
) -> Vec<ApprovalOption> {
    options
        .iter()
        .filter_map(|o| {
            let kind = match o.kind {
                PermissionOptionKind::AllowOnce => ApprovalOptionKind::AllowOnce,
                PermissionOptionKind::AllowAlways => ApprovalOptionKind::AllowAlways,
                PermissionOptionKind::RejectOnce => ApprovalOptionKind::RejectOnce,
                PermissionOptionKind::RejectAlways => ApprovalOptionKind::RejectAlways,
                _ => return None,
            };
            Some(ApprovalOption {
                option_id: o.option_id.0.to_string(),
                name: o.name.clone(),
                kind,
            })
        })
        .collect()
}

/// `requested` is an `option_id` the client picked off the agent's own labels
/// and is authoritative: an id matching nothing is a stale card, and answering
/// by kind would send an option the user never picked, so it resolves to
/// `None` and the caller cancels. Without one, the decision picks by kind.
pub(super) fn pick_option_id(
    options: &[agent_client_protocol::schema::v1::PermissionOption],
    decision: ApprovalDecision,
    requested: Option<&str>,
) -> Option<agent_client_protocol::schema::v1::PermissionOptionId> {
    if let Some(requested) = requested {
        return options
            .iter()
            .find(|o| o.option_id.0.as_ref() == requested)
            .map(|o| o.option_id.clone());
    }
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
        // Synthetic, from the daemon-restart rehydration sweep: the agent
        // never sees it, so the caller falls through to `Cancelled`.
        ApprovalDecision::Cancelled => &[][..],
    };
    for kind in preferred_kinds {
        if let Some(opt) = options.iter().find(|o| &o.kind == kind) {
            return Some(opt.option_id.clone());
        }
    }
    None
}

/// What an option actually stands for. A card answering by `option_id` sends
/// an allow-shaped decision alongside it, so without this a reject-kind option
/// would broadcast as an allow and leave the tool card running. `None` for a
/// kind newer than this build, which keeps the client's decision.
fn decision_for_option(
    options: &[agent_client_protocol::schema::v1::PermissionOption],
    option_id: &agent_client_protocol::schema::v1::PermissionOptionId,
) -> Option<ApprovalDecision> {
    let kind = options.iter().find(|o| &o.option_id == option_id)?.kind;
    match kind {
        PermissionOptionKind::AllowOnce => Some(ApprovalDecision::Allow),
        PermissionOptionKind::AllowAlways => Some(ApprovalDecision::AllowAlways),
        PermissionOptionKind::RejectOnce | PermissionOptionKind::RejectAlways => {
            Some(ApprovalDecision::Deny)
        }
        _ => None,
    }
}

/// Pairs the start frame `handle_permission_request` emits; without it a
/// denied tool hangs on "running" until the turn ends (#1713).
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

/// Clear the approval card and close the start frame `handle_permission_request`
/// emitted. Neither a cancel nor an unmatched option produces an agent
/// completion, so nothing else would terminate the tool card (#1713).
async fn cancel_approval(event_tx: &mpsc::Sender<Event>, nonce: &Nonce, tool_call_id: &str) {
    let _ = event_tx
        .send(Event::ApprovalResolved {
            nonce: nonce.clone(),
            decision: ApprovalDecision::Cancelled,
        })
        .await;
    emit_permission_denied(event_tx, tool_call_id, "permission cancelled").await;
}

pub(super) async fn handle_permission_request(
    request: RequestPermissionRequest,
    event_tx: mpsc::Sender<Event>,
    pending: PendingResponders,
    profile: &'static agent_profiles::AgentProfile,
    tool_context_cache: ToolContextCache,
) -> Result<RequestPermissionResponse, agent_client_protocol::Error> {
    let enter_ns = enter_timestamp_ns();
    let tool_call_id = request.tool_call.tool_call_id.0.to_string();
    trace!(
        target: "acp.protocol.tool_dispatch",
        handler = "permission_request",
        tool_call_id = %tool_call_id,
        enter_ns,
        "ACP request handler entered"
    );
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
    // Gemini's confirm-required tools routinely carry no raw_input (#1713), and
    // opencode reuses a tool_call_id whose earlier update had the command, so
    // merge the cached context before emitting events.
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
    // Gemini sends no standalone `tool_call` start frame, so without this the
    // approved tool would have no transcript card. The reducer dedupes
    // tool_start by id, so a later real start frame merges in place (#1713).
    let _ = event_tx
        .send(Event::ToolCallStarted {
            tool_call: tool_call.clone(),
        })
        .await;
    let offered = approval_options(&request.options);
    // A choice list (N same-kind options) must never be answered by kind: a
    // generic client rendered no labels and sent no option id, so by-kind
    // selection would answer the agent's first option for the user (#3741).
    let choice_list = is_choice_list(&offered);
    let approval = build_approval(tool_call, offered);
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
        return Ok(RequestPermissionResponse::new(
            RequestPermissionOutcome::Cancelled,
        ));
    }

    // #1147: comparing this against the exit trace shows how long approval
    // blocked the agent's turn.
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
        Ok(ApprovalResolutionMessage::Decision {
            decision,
            option_id: requested,
        }) => {
            // The generic allow/deny dialogs send no option id, so they cannot
            // answer a choice list; cancel so the user picks where the labels
            // are rendered.
            let choice_list_unanswered = requested.is_none() && choice_list;
            if let Some(option_id) =
                pick_option_id(&request.options, decision, requested.as_deref())
                    .filter(|_| !choice_list_unanswered)
            {
                // An option the client named outranks the decision it sent
                // with it: the option is what the user actually pressed.
                let decision = match requested {
                    Some(_) => {
                        decision_for_option(&request.options, &option_id).unwrap_or(decision)
                    }
                    None => decision,
                };
                let _ = event_tx
                    .send(Event::ApprovalResolved {
                        nonce: nonce.clone(),
                        decision,
                    })
                    .await;
                // A denied tool never runs, so close its start frame (#1713).
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
                    choice_list = choice_list_unanswered,
                    "no option matched (decision {decision:?}, requested {requested:?}); cancelling"
                );
                cancel_approval(&event_tx, &nonce, &tool_call_id).await;
                (RequestPermissionOutcome::Cancelled, "cancelled")
            }
        }
        // An explicit cancel_permission, or the resolver dropped on teardown.
        Ok(ApprovalResolutionMessage::Cancelled) | Err(_) => {
            cancel_approval(&event_tx, &nonce, &tool_call_id).await;
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
    Ok(RequestPermissionResponse::new(outcome))
}

/// `elicitation/create`, surfaced because we advertise `elicitation.form`.
/// Mirrors `handle_permission_request`. A cancel and an unparseable schema
/// both fall back to a graceful response so the agent's turn never hangs.
pub(super) async fn handle_elicitation_request(
    request: CreateElicitationRequest,
    event_tx: mpsc::Sender<Event>,
    pending: PendingResponders,
) -> Result<CreateElicitationResponse, agent_client_protocol::Error> {
    let nonce = Nonce::new();
    let elicitation = match parse_elicitation(nonce.clone(), &request, chrono::Utc::now()) {
        Ok(elicitation) => elicitation,
        Err(e) => {
            // Cancel, not Decline: the question was never shown, so "user
            // skipped" would misrepresent it.
            warn!(target: "acp.protocol", "unsupported elicitation, cancelling: {e}");
            return Ok(CreateElicitationResponse::new(ElicitationAction::Cancel));
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
        return Ok(CreateElicitationResponse::new(ElicitationAction::Cancel));
    }

    // `resolve_elicitation` validates before sending, so what arrives is
    // already a valid response. A dropped resolver cancels the tool call.
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

    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{PermissionOption, PermissionOptionId};

    fn option(id: &str, name: &str, kind: PermissionOptionKind) -> PermissionOption {
        PermissionOption::new(PermissionOptionId::new(id), name, kind)
    }

    /// In a question list every option is `allow_once`, so answering by kind
    /// would always send the first. The client's picked id wins, and an id
    /// belonging to no option resolves to nothing rather than a guess (#3741).
    #[test]
    fn picked_option_drives_id_and_decision() {
        let options: Vec<_> = ["Alpha", "Bravo", "Charlie", "Delta"]
            .iter()
            .enumerate()
            .map(|(i, name)| {
                option(
                    &format!("choice-{i}"),
                    name,
                    PermissionOptionKind::AllowOnce,
                )
            })
            .collect();

        let picked =
            pick_option_id(&options, ApprovalDecision::Allow, Some("choice-2")).expect("picked");
        assert_eq!(picked.0.as_ref(), "choice-2");
        assert!(pick_option_id(&options, ApprovalDecision::Allow, Some("choice-9")).is_none());
        // The kind-order fallback picks the first allow_once, the bug above.
        let fallback = pick_option_id(&options, ApprovalDecision::Allow, None).expect("fallback");
        assert_eq!(fallback.0.as_ref(), "choice-0");

        // Without a picked id the decision selects by kind, falling back to a
        // kind the agent did offer.
        let both = vec![
            option("yes", "Allow this once", PermissionOptionKind::AllowOnce),
            option("no", "Reject", PermissionOptionKind::RejectOnce),
        ];
        let always_only = vec![option(
            "always",
            "Always",
            PermissionOptionKind::AllowAlways,
        )];
        for (options, want) in [(&both, "yes"), (&always_only, "always")] {
            let id = pick_option_id(options, ApprovalDecision::Allow, None).unwrap();
            assert_eq!(id.0.as_ref(), want);
        }

        // The card sends an allow-shaped decision beside the option id, so the
        // recorded decision has to come from the option itself.
        {
            let options = vec![
                option("yes", "Yes", PermissionOptionKind::AllowOnce),
                option("forever", "Always", PermissionOptionKind::AllowAlways),
                option("no", "No", PermissionOptionKind::RejectOnce),
            ];
            for (id, expected) in [
                ("yes", ApprovalDecision::Allow),
                ("forever", ApprovalDecision::AllowAlways),
                ("no", ApprovalDecision::Deny),
            ] {
                let picked = PermissionOptionId::new(id);
                assert_eq!(
                    decision_for_option(&options, &picked),
                    Some(expected),
                    "{id}"
                );
            }
            let missing = PermissionOptionId::new("gone");
            assert_eq!(decision_for_option(&options, &missing), None);

            assert_eq!(
                approval_options(&options)
                    .into_iter()
                    .map(|o| (o.option_id, o.name, o.kind))
                    .collect::<Vec<_>>(),
                [
                    (
                        "yes".to_string(),
                        "Yes".to_string(),
                        ApprovalOptionKind::AllowOnce
                    ),
                    (
                        "forever".to_string(),
                        "Always".to_string(),
                        ApprovalOptionKind::AllowAlways
                    ),
                    (
                        "no".to_string(),
                        "No".to_string(),
                        ApprovalOptionKind::RejectOnce
                    ),
                ]
            );
        }
    }

    /// The generic allow/deny flow sends no option id, so a choice list must
    /// cancel rather than answer with the agent's first option. The guard is in
    /// `handle_permission_request`, so this drives the whole handler (#3741).
    #[tokio::test]
    async fn choice_list_with_no_picked_option_cancels_instead_of_picking_the_first() {
        use agent_client_protocol::schema::v1::{
            RequestPermissionRequest, ToolCallId, ToolCallUpdate, ToolCallUpdateFields,
        };
        use std::collections::HashMap;
        use std::sync::Arc;
        use tokio::sync::Mutex;

        let (event_tx, mut event_rx) = mpsc::channel(16);
        let pending: PendingResponders = Arc::new(Mutex::new(HashMap::new()));
        let request = RequestPermissionRequest::new(
            "s-choice",
            ToolCallUpdate::new(ToolCallId::new("t1"), ToolCallUpdateFields::default()),
            vec![
                option("choice-0", "First", PermissionOptionKind::AllowOnce),
                option("choice-1", "Second", PermissionOptionKind::AllowOnce),
            ],
        );
        let cache: crate::acp::acp_client::tool_context::ToolContextCache =
            Arc::new(std::sync::Mutex::new(
                crate::acp::acp_client::tool_context::ToolCallContextCache::default(),
            ));
        let handle = tokio::spawn(handle_permission_request(
            request,
            event_tx,
            pending.clone(),
            &crate::acp::agent_profiles::GEMINI,
            cache,
        ));
        let nonce = loop {
            if let Event::ApprovalRequested { approval } = event_rx.recv().await.expect("events") {
                break approval.nonce;
            }
        };
        // Generic answer: Allow with no option id, as the home dialog sends.
        let PendingResponder { resolver } = pending.lock().await.remove(&nonce).expect("parked");
        let PendingResolver::Approval(tx) = resolver else {
            panic!("approval resolver expected");
        };
        tx.send(ApprovalResolutionMessage::Decision {
            decision: ApprovalDecision::Allow,
            option_id: None,
        })
        .map_err(|_| "resolver gone")
        .unwrap();
        let response = handle.await.expect("handler task").expect("handler ok");
        assert!(
            matches!(response.outcome, RequestPermissionOutcome::Cancelled),
            "a choice list must not be answered by kind: {response:?}"
        );
        // The card closes on Cancelled, never on an Allow.
        loop {
            if let Event::ApprovalResolved { decision, .. } =
                event_rx.recv().await.expect("resolution event")
            {
                assert_eq!(decision, ApprovalDecision::Cancelled);
                break;
            }
        }
    }
}
