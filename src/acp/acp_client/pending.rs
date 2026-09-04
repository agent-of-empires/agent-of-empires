//! In-flight approval and elicitation responders, keyed by nonce.

use crate::acp::approvals::{ApprovalDecision, Nonce};
use crate::acp::elicitations::{Elicitation, ElicitationAnswer, ElicitationOutcome};
use agent_client_protocol::schema::v1::CreateElicitationResponse;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{oneshot, Mutex};

/// Resolution channel for a parked agent->client request awaiting a user
/// decision. Stored in the pending-responders map keyed by the structured
/// view's server-generated nonce. One map carries both permission
/// approvals and form elicitations; nonces are unique across both, and
/// the resolver variant records which kind of request is parked.
pub(super) struct PendingResponder {
    pub(super) resolver: PendingResolver,
}

pub(super) enum PendingResolver {
    /// `session/request_permission` awaiting allow/deny.
    Approval(oneshot::Sender<ApprovalResolutionMessage>),
    /// `elicitation/create` awaiting an accept/decline/cancel answer. The
    /// parsed form is kept so `resolve_elicitation` can validate the
    /// submitted answer BEFORE consuming the resolver: a validation
    /// failure then leaves the elicitation pending for a corrected
    /// resubmission instead of permanently cancelling it. The validated
    /// response (and its outcome) ride the oneshot so the parked callback
    /// just forwards them. Boxed to keep the enum small.
    Elicitation {
        elicitation: Box<Elicitation>,
        resolver: oneshot::Sender<ElicitationResolutionMessage>,
    },
}

/// Message sent over the resolver oneshot to unblock the parked
/// `on_receive_request` callback.
pub(super) enum ApprovalResolutionMessage {
    Decision { decision: ApprovalDecision },
    Cancelled,
}

/// Message sent over the elicitation resolver oneshot. Carries the
/// validated wire response for the agent, the outcome for status
/// derivation, and the display-ready answers for the transcript
/// (`Event::ElicitationResolved.answers`). See #2209.
pub(super) struct ElicitationResolutionMessage {
    pub(super) response: CreateElicitationResponse,
    pub(super) outcome: ElicitationOutcome,
    pub(super) answers: Vec<ElicitationAnswer>,
}

pub(super) type PendingResponders = Arc<Mutex<HashMap<Nonce, PendingResponder>>>;
