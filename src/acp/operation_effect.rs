use serde::{Deserialize, Serialize};

use super::establishment::NativeOperationCorrelation;
use super::event_sender::{EventMetadata, EventScope};
use super::protocol::{AcpOriginalRequestResult, AcpPromptObservation, AcpPromptWindowId};
use super::state::{Event, NativeEventOrigin};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativePromptResult {
    pub(crate) request_id: i64,
    pub(crate) origin: NativeEventOrigin,
    pub(crate) correlation: NativeOperationCorrelation,
    pub(crate) outcome: AcpOriginalRequestResult,
    pub(crate) window_id: AcpPromptWindowId,
    pub(crate) uncertain: bool,
}

impl NativePromptResult {
    pub(crate) fn from_metadata(metadata: &EventMetadata) -> Option<Self> {
        let request_id = metadata.native_prompt_id?;
        let origin = metadata.native_origin?;
        let operation = metadata.operation?;
        let outcome = operation.outcome?;
        let AcpPromptObservation::Written { window_id } = metadata.prompt_observation? else {
            return None;
        };
        Some(Self {
            request_id,
            origin,
            correlation: operation.correlation,
            outcome,
            window_id,
            uncertain: metadata.output_uncertainty.is_some(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ControlKind<'a> {
    Admission,
    Diagnostic,
    Stopped(&'a str),
    Quota,
    Prompt,
    DiffPrompt,
    AgentWork,
    Switched,
    Cleared,
    ContextReset,
    Unavailable { channel_loss: bool },
    StartupError,
    Resumed { manual: bool },
    ResourceLaunch,
    ResourceTrackingEnd,
    Other,
}

impl<'a> ControlKind<'a> {
    pub(crate) fn from_event(event: &'a Event) -> Self {
        match event {
            Event::ThinkingStarted => Self::Admission,
            Event::PromptRuntimeError { .. } => Self::Diagnostic,
            Event::Stopped { reason } => Self::Stopped(reason),
            Event::RateLimit { .. } => Self::Quota,
            Event::UserPromptSent { .. } => Self::Prompt,
            Event::UserDiffCommentsPrompt { .. } => Self::DiffPrompt,
            Event::AgentMessageChunk { .. } | Event::ToolCallStarted { .. } => Self::AgentWork,
            Event::AgentSwitched { .. } => Self::Switched,
            Event::SessionCleared => Self::Cleared,
            Event::SessionContextReset { .. } => Self::ContextReset,
            Event::AcpConnectionUnavailable { .. } => Self::Unavailable {
                channel_loss: event.is_channel_loss(),
            },
            Event::AgentStartupError { .. } => Self::StartupError,
            Event::RateLimitAutoResumed { manual, .. } => Self::Resumed { manual: *manual },
            Event::BackgroundAgentLaunched { .. } | Event::BackgroundTaskStarted { .. } => {
                Self::ResourceLaunch
            }
            Event::BackgroundTaskFinished { .. } => Self::ResourceTrackingEnd,
            Event::BackgroundAgentCompleted {
                status: crate::acp::state::BackgroundAgentStatus::Detached,
                tools,
                result,
                ..
            } if tools.is_empty() && result.is_none() => Self::ResourceTrackingEnd,
            _ => Self::Other,
        }
    }

    fn is_terminal(self) -> bool {
        matches!(self, Self::Diagnostic | Self::Stopped(_))
    }

    fn changes_conversation(self) -> bool {
        matches!(self, Self::Switched | Self::Cleared | Self::ContextReset)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ControlEffect {
    Current,
    Outcome,
    Refusal,
    Companion,
    Host,
    HostBoundary,
    Historical,
    Indeterminate,
}

impl ControlEffect {
    pub(crate) fn applies_state(self) -> bool {
        matches!(
            self,
            Self::Current | Self::Outcome | Self::Refusal | Self::HostBoundary
        )
    }
}

#[derive(Debug, Clone, Copy)]
enum Diagnostic {
    Native(NativePromptResult),
    Sdk(Option<NativeOperationCorrelation>),
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct OperationEffects {
    active: Option<NativeOperationCorrelation>,
    last_admission: Option<NativeOperationCorrelation>,
    diagnostic: Option<Diagnostic>,
}

impl OperationEffects {
    pub(crate) fn active(&self) -> Option<NativeOperationCorrelation> {
        self.active
    }

    pub(crate) fn apply(
        &mut self,
        kind: ControlKind<'_>,
        metadata: &EventMetadata,
    ) -> ControlEffect {
        let current = matches!(
            metadata.scope,
            EventScope::SessionCurrent | EventScope::OperationCurrent
        );
        let correlation = metadata.operation.map(|operation| operation.correlation);
        let owns = correlation.is_some() && correlation == self.active;
        let foreign = correlation.is_some() && self.active.is_some() && !owns;

        if metadata.host_action {
            if kind.changes_conversation() || matches!(kind, ControlKind::Stopped(_)) {
                self.active = None;
                self.diagnostic = None;
                return ControlEffect::HostBoundary;
            }
            if matches!(kind, ControlKind::Prompt | ControlKind::DiffPrompt) {
                self.diagnostic = None;
            }
            return if matches!(
                kind,
                ControlKind::Prompt | ControlKind::DiffPrompt | ControlKind::Resumed { .. }
            ) {
                ControlEffect::Host
            } else {
                ControlEffect::Historical
            };
        }

        if matches!(kind, ControlKind::Admission) && current {
            if correlation.is_some() {
                self.active = correlation;
                self.last_admission = correlation;
            }
            self.diagnostic = None;
            return ControlEffect::Current;
        }
        if matches!(kind, ControlKind::AgentWork | ControlKind::StartupError)
            && correlation.is_some()
            && !owns
        {
            return if self.last_admission.is_none() {
                ControlEffect::Indeterminate
            } else {
                ControlEffect::Historical
            };
        }

        if matches!(kind, ControlKind::Quota) && metadata.native_origin.is_some() {
            return if owns
                && metadata.operation.is_some_and(|operation| {
                    operation.outcome == Some(AcpOriginalRequestResult::Failed)
                })
                && matches!(
                    metadata.prompt_observation,
                    Some(AcpPromptObservation::Written { .. })
                ) {
                ControlEffect::Current
            } else if self.last_admission.is_none()
                && metadata.operation.is_some_and(|operation| {
                    operation.outcome == Some(AcpOriginalRequestResult::Failed)
                })
                && matches!(
                    metadata.prompt_observation,
                    Some(AcpPromptObservation::Written { .. })
                )
            {
                ControlEffect::Indeterminate
            } else {
                ControlEffect::Historical
            };
        }

        if kind.is_terminal() {
            if foreign {
                return ControlEffect::Historical;
            }
            if metadata.native_origin.is_some()
                && metadata
                    .operation
                    .is_none_or(|operation| operation.outcome.is_none())
                && !owns
            {
                return ControlEffect::Historical;
            }
            let original = NativePromptResult::from_metadata(metadata);
            if matches!(kind, ControlKind::Stopped(_)) {
                let companion = match self.diagnostic {
                    Some(Diagnostic::Native(known)) => original == Some(known),
                    Some(Diagnostic::Sdk(known)) => {
                        metadata.native_origin.is_none() && correlation == known && current
                    }
                    None => false,
                };
                if companion {
                    self.diagnostic = None;
                    return ControlEffect::Companion;
                }
            }
            if metadata.native_origin.is_some()
                && metadata
                    .operation
                    .is_some_and(|operation| operation.outcome.is_some())
            {
                if !owns {
                    return if self.last_admission.is_none() {
                        ControlEffect::Indeterminate
                    } else {
                        ControlEffect::Historical
                    };
                }
            } else if !(current || owns)
                || (correlation.is_some() && correlation == self.last_admission && !owns)
            {
                return ControlEffect::Historical;
            }
            self.active = None;
            let refusal = metadata.native_origin.is_some()
                && metadata
                    .operation
                    .is_none_or(|operation| operation.outcome.is_none());
            self.diagnostic = if matches!(kind, ControlKind::Diagnostic) && !refusal {
                original.map(Diagnostic::Native).or_else(|| {
                    metadata
                        .native_origin
                        .is_none()
                        .then_some(Diagnostic::Sdk(correlation))
                })
            } else {
                None
            };
            return if refusal {
                ControlEffect::Refusal
            } else {
                ControlEffect::Outcome
            };
        }

        if matches!(
            kind,
            ControlKind::Unavailable {
                channel_loss: false
            }
        ) {
            return if current {
                ControlEffect::Current
            } else {
                ControlEffect::Historical
            };
        }
        if current {
            if kind.changes_conversation()
                || matches!(kind, ControlKind::Unavailable { channel_loss: true })
            {
                self.active = None;
                self.diagnostic = None;
            }
            ControlEffect::Current
        } else {
            ControlEffect::Historical
        }
    }
}
