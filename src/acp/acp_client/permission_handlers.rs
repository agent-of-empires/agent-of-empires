//! `session/request_permission` and elicitation requests, which aoe answers
//! from the approval policy or by asking the user.

use crate::acp::agent_profiles;
use crate::acp::approvals::{
    is_choice_list, ApprovalDecision, ApprovalOption, ApprovalOptionKind, Nonce,
};
use crate::acp::elicitations::{parse_elicitation, ElicitationOutcome};
use crate::acp::event_sender::EventSender;
use crate::acp::permissions::build_approval;
use crate::acp::state::{Event, ToolCall};
use agent_client_protocol::schema::v1::{
    CreateElicitationRequest, CreateElicitationResponse, ElicitationAction, PermissionOptionKind,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome,
};
use std::sync::Arc;

#[cfg(test)]
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tracing::{trace, warn};

use super::fs_handlers::enter_timestamp_ns;
use super::pending::{
    ApprovalResolutionMessage, ElicitationResolutionMessage, PendingResolver, PendingResponder,
    PendingResponders,
};
use super::plan::approval_plan_text;
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
    event_tx: &EventSender,
    tool_call_id: &str,
    content: &str,
) {
    let _ = event_tx
        .historical()
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
async fn cancel_approval(event_tx: &EventSender, nonce: &Nonce, tool_call_id: &str) {
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
    event_tx: EventSender,
    pending: PendingResponders,
    profile: &'static agent_profiles::AgentProfile,
    tool_context_cache: Option<ToolContextCache>,
    native_callback: Option<Arc<super::session_identity::CapturedCallback>>,
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
    let cached_raw_input = tool_context_cache.as_ref().and_then(|cache| {
        cache
            .lock()
            .expect("tool context cache mutex poisoned")
            .get(&tool_call_id)
    });
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
        .historical()
        .send(Event::ToolCallStarted {
            tool_call: tool_call.clone(),
        })
        .await;
    let offered = approval_options(&request.options);
    // A choice list (N same-kind options) must never be answered by kind: a
    // generic client rendered no labels and sent no option id, so by-kind
    // selection would answer the agent's first option for the user (#3741).
    let choice_list = is_choice_list(&offered);
    let mut approval = build_approval(tool_call, offered);
    // A destructive approval keeps the card's hold-to-allow, which the plan
    // modal's Allow would bypass.
    if !approval.destructive {
        approval.plan = approval_plan_text(profile, enriched_raw_input.as_ref());
    }
    let nonce = approval.nonce.clone();

    let event = Event::ApprovalRequested { approval };
    let (publication, event) = match &native_callback {
        Some(callback) => (
            Some(Arc::new(super::pending::PendingRequestPublication::new(
                Arc::new(event),
                Arc::clone(callback),
                callback.connection.clone(),
                &pending,
            ))),
            None,
        ),
        None => (None, Some(event)),
    };
    let (resolve_tx, resolve_rx) = oneshot::channel::<ApprovalResolutionMessage>();
    let published = match event_tx.reserve().await {
        Ok(permit) => {
            let (mut responders, _decision) = match &publication {
                Some(publication) => {
                    let (responders, native, registry) = publication
                        .lock_responders_for_publication(&pending)
                        .await
                        .map_err(|_| super::session_identity::SessionIngress::request_refused())?;
                    (responders, Some((native, registry)))
                }
                None => (pending.lock().await, None),
            };
            responders.insert(
                nonce.clone(),
                PendingResponder {
                    resolver: PendingResolver::Approval(resolve_tx),
                    publication: publication.clone(),
                },
            );
            if let Some(publication) = &publication {
                permit.send_pending_request(Arc::clone(publication));
            } else {
                permit.send(event.expect("non-Native request retains its owned payload"));
            }
            true
        }
        Err(_) => false,
    };
    if !published {
        if publication.is_some() {
            return Err(agent_client_protocol::util::internal_error(
                "native approval request could not be published",
            ));
        }
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
        Err(_)
            if publication
                .as_ref()
                .is_some_and(|publication| !publication.published_at(u64::MAX)) =>
        {
            return Err(agent_client_protocol::util::internal_error(
                "native approval request could not be published",
            ));
        }
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
    if let Some(publication) = &publication {
        publication
            .wait_for_publication_guard()
            .await
            .map_err(|_| super::session_identity::SessionIngress::request_refused())?;
    }
    Ok(RequestPermissionResponse::new(outcome))
}

/// `elicitation/create`, surfaced because we advertise `elicitation.form`.
/// Mirrors `handle_permission_request`. A cancel and an unparseable schema
/// both fall back to a graceful response so the agent's turn never hangs.
pub(super) async fn handle_elicitation_request(
    request: CreateElicitationRequest,
    event_tx: EventSender,
    pending: PendingResponders,
    native_callback: Option<Arc<super::session_identity::CapturedCallback>>,
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
    let event = Event::ElicitationRequested {
        elicitation: elicitation.clone(),
    };
    let (publication, event) = match &native_callback {
        Some(callback) => (
            Some(Arc::new(super::pending::PendingRequestPublication::new(
                Arc::new(event),
                Arc::clone(callback),
                callback.connection.clone(),
                &pending,
            ))),
            None,
        ),
        None => (None, Some(event)),
    };

    let (resolve_tx, resolve_rx) = oneshot::channel::<ElicitationResolutionMessage>();
    // Request purpose was frozen at native framing, before any queue or wait.
    let published = match event_tx.reserve().await {
        Ok(permit) => {
            let (mut responders, _decision) = match &publication {
                Some(publication) => {
                    let (responders, native, registry) = publication
                        .lock_responders_for_publication(&pending)
                        .await
                        .map_err(|_| super::session_identity::SessionIngress::request_refused())?;
                    (responders, Some((native, registry)))
                }
                None => (pending.lock().await, None),
            };
            responders.insert(
                nonce.clone(),
                PendingResponder {
                    resolver: PendingResolver::Elicitation {
                        elicitation: Box::new(elicitation.clone()),
                        resolver: resolve_tx,
                    },
                    publication: publication.clone(),
                },
            );
            if let Some(publication) = &publication {
                permit.send_pending_request(Arc::clone(publication));
            } else {
                permit.send(event.expect("non-Native request retains its owned payload"));
            }
            true
        }
        Err(_) => false,
    };
    if !published {
        if publication.is_some() {
            return Err(agent_client_protocol::util::internal_error(
                "native elicitation request could not be published",
            ));
        }
        return Ok(CreateElicitationResponse::new(ElicitationAction::Cancel));
    }

    // A journal failure is a host refusal, not a user cancellation.
    let ElicitationResolutionMessage {
        response,
        outcome,
        answers,
    } = match resolve_rx.await {
        Ok(resolution) => resolution,
        Err(_)
            if publication
                .as_ref()
                .is_some_and(|publication| !publication.published_at(u64::MAX)) =>
        {
            return Err(agent_client_protocol::util::internal_error(
                "native elicitation request could not be published",
            ));
        }
        Err(_) => ElicitationResolutionMessage {
            response: CreateElicitationResponse::new(ElicitationAction::Cancel),
            outcome: ElicitationOutcome::Cancelled,
            answers: Vec::new(),
        },
    };

    let _ = event_tx
        .send(Event::ElicitationResolved {
            nonce: nonce.clone(),
            outcome,
            answers,
        })
        .await;

    // A user answer must not become a decision for an owner retired while waiting.
    if let Some(publication) = &publication {
        publication
            .wait_for_publication_guard()
            .await
            .map_err(|_| super::session_identity::SessionIngress::request_refused())?;
    }
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
            event_tx.into(),
            pending.clone(),
            &crate::acp::agent_profiles::GEMINI,
            Some(cache),
            None,
        ));
        let nonce = loop {
            let crate::acp::event_sender::SdkEvent::Data(data) =
                event_rx.recv().await.expect("events")
            else {
                panic!("unexpected control event");
            };
            if let Event::ApprovalRequested { approval } = data.event.as_ref() {
                break approval.nonce.clone();
            }
        };
        // Generic answer: Allow with no option id, as the home dialog sends.
        let PendingResponder { resolver, .. } =
            pending.lock().await.remove(&nonce).expect("parked");
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
            let crate::acp::event_sender::SdkEvent::Data(data) =
                event_rx.recv().await.expect("resolution event")
            else {
                panic!("unexpected control event");
            };
            if let Event::ApprovalResolved { decision, .. } = data.event.as_ref() {
                assert_eq!(*decision, ApprovalDecision::Cancelled);
                break;
            }
        }
    }
    /// The plan modal's Allow is a single click, so a destructive approval must
    /// not carry a plan even when its input has a `plan` field.
    #[tokio::test]
    async fn plan_is_only_attached_to_non_destructive_approvals() {
        use agent_client_protocol::schema::v1::{
            RequestPermissionRequest, ToolCallId, ToolCallUpdate, ToolCallUpdateFields,
        };
        use std::collections::HashMap;
        use std::sync::Arc;
        use tokio::sync::Mutex;

        for (title, raw_input, want_plan) in [
            (
                "Ready to code?",
                serde_json::json!({ "plan": "# Plan\nprose" }),
                true,
            ),
            (
                "Bash",
                serde_json::json!({ "command": "rm -rf /tmp/x", "plan": "# Plan" }),
                false,
            ),
        ] {
            let (event_tx, mut event_rx) = mpsc::channel(16);
            let pending: PendingResponders = Arc::new(Mutex::new(HashMap::new()));
            let request = RequestPermissionRequest::new(
                "s-plan",
                ToolCallUpdate::new(
                    ToolCallId::new("t1"),
                    ToolCallUpdateFields::new()
                        .title(title)
                        .raw_input(raw_input),
                ),
                vec![
                    option("allow", "Allow", PermissionOptionKind::AllowOnce),
                    option("reject", "Reject", PermissionOptionKind::RejectOnce),
                ],
            );
            let cache: crate::acp::acp_client::tool_context::ToolContextCache =
                Arc::new(std::sync::Mutex::new(
                    crate::acp::acp_client::tool_context::ToolCallContextCache::default(),
                ));
            let handle = tokio::spawn(handle_permission_request(
                request,
                event_tx.into(),
                pending,
                &crate::acp::agent_profiles::CLAUDE,
                Some(cache),
                None,
            ));
            let has_plan = loop {
                let crate::acp::event_sender::SdkEvent::Data(data) =
                    event_rx.recv().await.expect("events")
                else {
                    panic!("unexpected control event");
                };
                if let Event::ApprovalRequested { approval } = data.event.as_ref() {
                    break approval.plan.is_some();
                }
            };
            handle.abort();
            assert_eq!(has_plan, want_plan, "{title}");
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn managed_native_unpublished_decisions_refuse_without_user_outcomes() {
        use crate::acp::supervisor::test_support::managed::{with_managed, ManagedClient};
        use axum::extract::{Path, State};
        use axum::http::StatusCode;
        use axum::response::IntoResponse;
        use axum::Json;
        use serde_json::json;
        use std::time::Duration;

        struct PublicationFault {
            state: Arc<crate::server::AppState>,
            id: String,
            status: Option<crate::session::Status>,
            sqlite: bool,
        }

        impl PublicationFault {
            fn restore(&mut self) -> anyhow::Result<()> {
                if let Some(status) = self.status {
                    crate::session::Storage::open_unwatched("test")?.update(|rows, _| {
                        let row = rows
                            .iter_mut()
                            .find(|row| row.id == self.id)
                            .ok_or(crate::session::SessionGone)?;
                        anyhow::ensure!(
                            row.status == crate::session::Status::Stopped || row.status == status,
                            crate::session::LifecycleReservationError::Superseded
                        );
                        row.status = status;
                        Ok(())
                    })?;
                    self.status = None;
                }
                if self.sqlite {
                    self.state
                        .acp_event_store
                        .test_conn()
                        .execute_batch("DROP TRIGGER IF EXISTS reject_pending_request;")?;
                    self.sqlite = false;
                }
                Ok(())
            }
        }

        impl Drop for PublicationFault {
            fn drop(&mut self) {
                if let Err(error) = self.restore() {
                    eprintln!("owned publication fault restoration failed: {error}");
                }
            }
        }

        async fn assert_unresolvable(client: &ManagedClient, nonce: &Nonce, question: bool) {
            let state = State(client.app_state());
            let path = Path((client.id.clone(), nonce.0.clone()));
            let response = if question {
                crate::server::api::resolve_elicitation(
                    state,
                    path,
                    Ok(Json(
                        serde_json::from_value(json!({
                            "action": "accept", "answers": {"name": "Not published"}
                        }))
                        .unwrap(),
                    )),
                )
                .await
                .into_response()
            } else {
                crate::server::api::resolve_approval(
                    state,
                    path,
                    Ok(Json(
                        serde_json::from_value(json!({"decision": "Allow"})).unwrap(),
                    )),
                )
                .await
                .into_response()
            };
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
        }

        with_managed(|scope| Box::pin(async move {
            for original in [true, false] {
                for question in [false, true] {
                    for discard in [false, true] {
                        tokio::time::timeout(Duration::from_secs(30), async {
                            let directory = scope.tempdir();
                            let sid = uuid::Uuid::new_v4().to_string();
                            let callback_id = format!("callback-{sid}");
                            let (constructor_tx, constructor_rx) = oneshot::channel();
                            let (prompt_tx, prompt_rx) = oneshot::channel();
                            let mut constructor_tx = Some(constructor_tx);
                            let mut prompt_tx = Some(prompt_tx);
                            let (config, mut native) = scope.native_peer(
                                directory.path().into(), "codex", "codex", move |request| {
                                    let id = request.get("id")?;
                                    match request["method"].as_str().unwrap() {
                                        "initialize" => Some(json!({"jsonrpc":"2.0","id":id,
                                            "result":{"protocolVersion":1,"agentCapabilities":{}}})),
                                        "session/new" => {
                                            constructor_tx.take().unwrap().send(id.clone()).unwrap();
                                            None
                                        }
                                        "session/prompt" => {
                                            prompt_tx.take().unwrap().send(id.clone()).unwrap();
                                            None
                                        }
                                        "session/cancel" => None,
                                        other => panic!("unexpected Native operation: {other}"),
                                    }
                                },
                            );
                            let mut client = scope.spawn(config, "unpublished-decision").await.unwrap();
                            let constructor_id = constructor_rx.await.unwrap();
                            let mut prompt_id = None;
                            if !original {
                                native.send(json!({"jsonrpc":"2.0","id":constructor_id,
                                    "result":{"sessionId":sid}})).await;
                                client.ready().await;
                                client.send_prompt("one explicit prompt", &[]).await.unwrap();
                                prompt_id = Some(prompt_rx.await.unwrap());
                            }
                            let state = client.app_state();
                            let actual = client.actual();
                            let owner = actual.connected_runner().unwrap();
                            let pause = client.pause_drain().await;
                            let prefix = state.acp_event_store.highest_seq(&client.id);
                            let params = if question {
                                serde_json::to_value(CreateElicitationRequest::new(
                                    agent_client_protocol::schema::v1::ElicitationFormMode::new(
                                        agent_client_protocol::schema::v1::ElicitationSessionScope::new(sid.clone()),
                                        agent_client_protocol::schema::v1::ElicitationSchema::new().string("name", true),
                                    ),
                                    "A question the host cannot publish",
                                )).unwrap()
                            } else {
                                json!({"sessionId":sid,
                                    "toolCall":{"toolCallId":"unpublished-tool","title":"Unpublished tool"},
                                    "options":[{"optionId":"allow","name":"Allow","kind":"allow_once"}]})
                            };
                            native.send(json!({"jsonrpc":"2.0","id":callback_id,
                                "method":if question { "elicitation/create" } else { "session/request_permission" },
                                "params":params})).await;
                            // The real handler parked its real responder before the drain's dequeue.
                            let (nonce, publication) = loop {
                                let responders = actual.pending_responders.lock().await;
                                if let Some((nonce, responder)) = responders.iter().next() {
                                    break (nonce.clone(), responder.publication.clone().unwrap());
                                }
                                drop(responders);
                                tokio::task::yield_now().await;
                            };
                            assert_eq!(publication.is_original(), original);
                            assert!(!publication.published_at(u64::MAX));
                            assert_unresolvable(&client, &nonce, question).await;
                            let storage = crate::session::Storage::open_unwatched("test").unwrap();
                            let status = storage.load_instances_complete().unwrap().into_iter()
                                .find(|row| row.id == client.id).unwrap().status;
                            let mut fault = PublicationFault {
                                state: Arc::clone(&state),
                                id: client.id.clone(),
                                status: discard.then_some(status),
                                sqlite: !discard,
                            };
                            if discard {
                                storage.update(|rows, _| {
                                    rows.iter_mut().find(|row| row.id == client.id).unwrap().status = crate::session::Status::Stopped;
                                    Ok(())
                                }).unwrap();
                            } else {
                                // Use the existing SQLite fault seam, not a sink that pretends to fail.
                                state.acp_event_store.test_conn().execute_batch(
                                    "CREATE TEMP TRIGGER reject_pending_request BEFORE INSERT ON acp_events
                                     WHEN NEW.discriminant IN ('ApprovalRequested','ElicitationRequested')
                                     BEGIN SELECT RAISE(ABORT,'pending request append fault'); END;"
                                ).unwrap();
                            }
                            drop(pause);
                            let refused = native.reply().await;
                            assert_eq!(refused["id"], callback_id, "{refused}");
                            assert_eq!(refused["error"]["code"], -32603, "{refused}");
                            assert!(refused.get("result").is_none(), "host refusal is not a user decision: {refused}");
                            assert!(actual.pending_responders.lock().await.is_empty());
                            assert!(!publication.published_at(u64::MAX));
                            assert_eq!(actual.connected_runner(), Some(owner));
                            assert_unresolvable(&client, &nonce, question).await;
                            if !discard {
                                loop {
                                    match client.next_event().await.expect("public append failure") {
                                        Event::SessionNotice { severity, .. } if severity == "error" => {
                                            break;
                                        }
                                        Event::ApprovalRequested { .. } | Event::ElicitationRequested { .. } =>
                                            panic!("unpublished request must not grant a UI capability"),
                                        _ => {}
                                    }
                                }
                            }
                            fault.restore().unwrap();
                            let reduced = state.session_service.fold_control_state(&client.id).await.unwrap();
                            assert!(reduced.pending_approvals.is_empty());
                            assert!(reduced.pending_elicitations.is_empty());
                            assert!(reduced.original_decision_nonces.is_empty());
                            let suffix = state.acp_event_store.replay_from(&client.id, prefix).unwrap();
                            assert!(!suffix.iter().any(|stored| matches!(stored.event.as_ref(),
                                Event::ApprovalRequested { .. } | Event::ElicitationRequested { .. }
                                | Event::ApprovalResolved { .. } | Event::ElicitationResolved { .. }
                                | Event::SessionContextReset { .. })));
                            assert!(state.acp_supervisor.take_startup_requeues().is_empty());
                            if original {
                                assert!(!reduced.connection_ready);
                                assert_eq!(native.requests(), ["initialize", "session/new"]);
                                native.send(json!({"jsonrpc":"2.0","id":constructor_id,
                                    "result":{"sessionId":sid}})).await;
                                let ready = state.acp_supervisor.wait_until_ready(&client.id).await;
                                assert!(matches!(ready,
                                    Err(crate::acp::supervisor::SupervisorError::Establishment(
                                        crate::acp::establishment::EstablishmentRefusal::Unconfirmed
                                            | crate::acp::establishment::EstablishmentRefusal::Superseded
                                    ))
                                ), "an uncertified Original prefix cannot grant readiness");
                                assert!(actual.establishment_projection().unwrap().applied().is_none());
                            } else {
                                assert_eq!(native.requests(), ["initialize", "session/new", "session/prompt"]);
                                if !discard {
                                    native.send(json!({"jsonrpc":"2.0","id":prompt_id.unwrap(),
                                        "result":{"stopReason":"end_turn"}})).await;
                                    while !matches!(client.next_event().await.expect("genuine prompt completion"), Event::Stopped { .. }) {}
                                }
                            }
                            client.shutdown().await.unwrap();
                        }).await.expect("actual unpublished Native callback must receive its host refusal");
                    }
                }
            }
        })).await;
    }
    #[tokio::test]
    #[serial_test::serial]
    async fn managed_native_original_decisions_do_not_grant_current_and_current_resolves_through_http(
    ) {
        use crate::acp::elicitations::ElicitationResolution;
        use crate::acp::supervisor::test_support::managed::with_managed;
        use axum::extract::{Path, State};
        use axum::http::StatusCode;
        use axum::response::IntoResponse;
        use axum::Json;
        use serde_json::json;
        use std::time::Duration;

        with_managed(|scope| {
            Box::pin(async move {
                for target in ["load", "new"] {
                let mut stage = "Native constructor request";
                let observed = tokio::time::timeout(Duration::from_secs(30), async {
                    let directory = scope.tempdir();
                    let original_file = directory.path().join("constructor-original.txt");
                    std::fs::write(&original_file, "Original constructor I/O").unwrap();
                    let original_id = if target == "load" {
                        let (config, original_native) = scope.native_peer(
                            directory.path().into(),
                            "codex",
                            "codex",
                            |request| {
                                let id = request.get("id")?;
                                Some(match request["method"].as_str().unwrap() {
                                    "initialize" => json!({"jsonrpc":"2.0","id":id,
                                        "result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true}}}),
                                    "session/new" => json!({"jsonrpc":"2.0","id":id,
                                        "result":{"sessionId":"elicitation-native"}}),
                                    method => panic!("unexpected priming Native request: {method}"),
                                })
                            },
                        );
                        let original = scope.spawn(config, "elicitation-prime").await.unwrap();
                        original.ready().await;
                        let id = original.id.clone();
                        original.stop_session().await;
                        assert_eq!(original_native.requests(), ["initialize", "session/new"]);
                        Some(id)
                    } else {
                        None
                    };
                    let constructor_method = if target == "load" {
                        "session/load"
                    } else {
                        "session/new"
                    };
                    let (new_seen, new_received) = oneshot::channel();
                    let (prompt_seen, prompt_received) = oneshot::channel();
                    let mut new_seen = Some(new_seen);
                    let mut prompt_seen = Some(prompt_seen);
                    let (config, mut native) = scope.native_peer(
                        directory.path().into(),
                        "codex",
                        "codex",
                        move |request| {
                            let id = request.get("id")?;
                            match request["method"].as_str().unwrap() {
                                "initialize" => Some(json!({
                                    "jsonrpc": "2.0", "id": id,
                                    "result": {"protocolVersion": 1, "agentCapabilities": {"loadSession": true}}
                                })),
                                method if method == constructor_method => {
                                    new_seen.take().unwrap().send(id.clone()).unwrap();
                                    None
                                }
                                "session/prompt" => {
                                    prompt_seen.take().unwrap().send(id.clone()).unwrap();
                                    None
                                }
                                other => panic!("unexpected managed native operation: {other}"),
                            }
                        },
                    );
                    let mut client = match original_id {
                        Some(id) => scope.resume_stopped(&id, config).await,
                        None => scope.spawn(config, "native-elicitation").await.unwrap(),
                    };
                    let constructor_id = new_received.await.unwrap();
                    stage = "Original constructor I/O";
                    native
                        .send(json!({
                            "jsonrpc": "2.0", "id": "pending-original-read",
                            "method": "fs/read_text_file",
                            "params": {"sessionId": "elicitation-native", "path": original_file}
                        }))
                        .await;
                    let original_read = native.reply().await;
                    assert_eq!(original_read["id"], "pending-original-read");
                    assert_eq!(original_read["result"]["content"], "Original constructor I/O", "{original_read}");
                    stage = "Original decisions without Current authority";
                    let state = client.app_state();
                    assert!(!state.session_service.fold_control_state(&client.id).await.unwrap().connection_ready);
                    assert!(client.send_prompt("must not become a Current prompt", &[]).await.is_err());
                    let question = CreateElicitationRequest::new(
                        agent_client_protocol::schema::v1::ElicitationFormMode::new(
                            agent_client_protocol::schema::v1::ElicitationSessionScope::new(
                                "elicitation-native",
                            ),
                            agent_client_protocol::schema::v1::ElicitationSchema::new()
                                .string("name", true),
                        ),
                        "Please enter your name",
                    );
                    for (request_id, method, params) in [
                        ("pending-permission", "session/request_permission", json!({
                            "sessionId": "elicitation-native",
                            "toolCall": {"toolCallId": "pending-tool", "title": "Pending tool"},
                            "options": [{"optionId": "allow", "name": "Allow", "kind": "allow_once"}]
                        })),
                        ("pending-question", "elicitation/create", serde_json::to_value(&question).unwrap()),
                    ] {
                        native
                            .send(json!({
                                "jsonrpc": "2.0", "id": request_id,
                                "method": method, "params": params
                            }))
                            .await;
                        let nonce = loop {
                            tokio::select! {
                                response = native.reply() => panic!("Original question returned before a human answer: {response}"),
                                event = client.next_event() => match event.expect("pending constructor publication") {
                                    Event::ApprovalRequested { approval } => {
                                        assert_eq!(request_id, "pending-permission");
                                        break approval.nonce;
                                    }
                                    Event::ElicitationRequested { elicitation } => {
                                        assert_eq!(request_id, "pending-question");
                                        break elicitation.nonce;
                                    }
                                    Event::AcpConnectionUnavailable { .. } => panic!("original Native must remain connected"),
                                    _ => {}
                                }
                            }
                        };
                        let publication = client.actual().pending_responders.lock().await
                            .get(&nonce).unwrap().publication.clone().expect("actual Native request custody");
                        assert!(publication.is_original());
                        assert_eq!(publication.metadata().scope, crate::acp::event_sender::EventScope::HistoryOnly);
                        let reduced = state.session_service.fold_control_state(&client.id).await.unwrap();
                        assert!(!reduced.connection_ready);
                        assert_eq!(reduced.original_decision_nonces.as_slice(), std::slice::from_ref(&nonce));
                        if request_id == "pending-permission" {
                            client.actual().resolve_permission(nonce, ApprovalDecision::Allow, None).await.unwrap();
                        } else {
                            client.actual().resolve_elicitation(nonce, serde_json::from_value(json!({
                                "action": "accept", "answers": {"name": "Original"}
                            })).unwrap()).await.unwrap();
                        }
                        let answered = native.reply().await;
                        assert_eq!(answered["id"], request_id);
                        assert!(answered.get("error").is_none(), "{answered}");
                        assert!(answered.get("result").is_some(), "{answered}");
                    }
                    // Answering Original requests never establishes Current readiness.
                    assert!(client.actual().pending_responders.lock().await.is_empty());
                    let reduced = state
                        .session_service
                        .fold_control_state(&client.id)
                        .await
                        .unwrap();
                    assert!(reduced.pending_elicitations.is_empty());
                    assert!(!reduced.connection_ready);
                    assert!(reduced.original_decision_nonces.is_empty());
                    assert!(state.acp_event_store.pending_approval_requests(&client.id).is_empty());

                    stage = "genuine constructor readiness";
                    native
                        .send(json!({
                            "jsonrpc": "2.0", "id": constructor_id,
                            "result": if target == "load" { json!({}) } else { json!({"sessionId": "elicitation-native"}) }
                        }))
                        .await;
                    client.ready().await;
                    stage = "Native prompt request";
                    client
                        .send_prompt("answer the native question", &[])
                        .await
                        .unwrap();
                    let prompt_id = prompt_received.await.unwrap();
                    stage = "Current question publication";
                    native
                        .send(json!({
                            "jsonrpc": "2.0", "id": "current-question",
                            "method": "elicitation/create", "params": question
                        }))
                        .await;
                    let elicitation = loop {
                        tokio::select! {
                            response = native.reply() => {
                                panic!("Current question returned before user resolution: {response}")
                            }
                            event = client.next_event() => match event.expect("managed publication") {
                                Event::ElicitationRequested { elicitation } => break elicitation,
                                Event::AgentStartupError { message, .. } => panic!("{message}"),
                                Event::AcpConnectionUnavailable { .. } => {
                                    panic!("original Native must remain connected")
                                }
                                _ => {}
                            }
                        }
                    };
                    let reduced = state
                        .session_service
                        .fold_control_state(&client.id)
                        .await
                        .unwrap();
                    assert_eq!(reduced.pending_elicitations.len(), 1);
                    assert_eq!(reduced.pending_elicitations[0], elicitation);
                    assert_eq!(elicitation.message, "Please enter your name");
                    stage = "HTTP resolution";
                    let resolution: ElicitationResolution = serde_json::from_value(json!({
                        "action": "accept", "answers": {"name": "Ada"}
                    }))
                    .unwrap();
                    let response = crate::server::api::resolve_elicitation(
                        State(state.clone()),
                        Path((client.id.clone(), elicitation.nonce.0.clone())),
                        Ok(Json(resolution)),
                    )
                    .await
                    .into_response();
                    assert_eq!(response.status(), StatusCode::NO_CONTENT);
                    stage = "Native answer";
                    let answered = native.reply().await;
                    assert_eq!(answered["id"], "current-question");
                    assert_eq!(answered["result"]["action"], "accept", "{answered}");
                    assert_eq!(answered["result"]["content"]["name"], "Ada", "{answered}");
                    stage = "resolution publication";
                    loop {
                        if let Event::ElicitationResolved { nonce, .. } =
                            client.next_event().await.expect("managed resolution")
                        {
                            assert_eq!(nonce, elicitation.nonce);
                            break;
                        }
                    }
                    let reduced = state
                        .session_service
                        .fold_control_state(&client.id)
                        .await
                        .unwrap();
                    assert!(reduced.pending_elicitations.is_empty());
                    assert!(client.actual().pending_responders.lock().await.is_empty());
                    stage = "genuine prompt completion";
                    native
                        .send(json!({
                            "jsonrpc": "2.0", "id": prompt_id,
                            "result": {"stopReason": "end_turn"}
                        }))
                        .await;
                    loop {
                        if let Event::Stopped { .. } = client
                            .next_event()
                            .await
                            .expect("managed prompt completion")
                        {
                            break;
                        }
                    }
                    assert_eq!(
                        state.acp_event_store.has_in_flight_main_turn(&client.id).unwrap(),
                        crate::acp::event_store::ControlKnowledge::Known(false)
                    );
                    assert_eq!(
                        native.requests(),
                        ["initialize", constructor_method, "session/prompt"]
                    );
                    stage = "owned shutdown";
                    client.shutdown().await.unwrap();
                })
                .await;
                observed.unwrap_or_else(|_| panic!("managed elicitation timed out during {stage}"));
                }
            })
        })
        .await;
    }
}
