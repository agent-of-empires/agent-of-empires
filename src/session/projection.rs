//! The session facts every read surface (CLI, runtime wire, REST) shares.
//!
//! Borrowed and typed so each surface formats timestamps and names fields to
//! its own contract without a second derivation from the `Instance`.

use chrono::{DateTime, Utc};

use super::{Instance, SessionBucket, Status, WorkspaceRepo, WorktreeInfo};

/// Explicit user intent. The selection token is not Native authority.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum SessionRecoveryAction {
    Same,
    Fresh,
}

#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct SessionRecoveryRequest {
    pub action: SessionRecoveryAction,
    pub expected: String,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum SessionRecoveryReason {
    OperationFailed,
    ChannelUnavailable,
    ProbeFailed,
    IdentityUnattested,
    RuntimeExited,
    ObservationUnknown,
}

#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
pub struct RecoveryActions {
    pub attach: bool,
    pub retry_operation: bool,
    pub restart_same: bool,
    pub stop: bool,
    pub fresh: bool,
}

#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct SessionRecovery {
    pub reason: SessionRecoveryReason,
    pub diagnostic: String,
    pub evidence: Option<super::OperationErrorEvidence>,
    pub runtime: super::RuntimeEvidence,
    pub work_in_progress: bool,
    pub uncertainty: Option<String>,
    pub guidance: std::borrow::Cow<'static, str>,
    pub actions: RecoveryActions,
    pub expected: String,
}

/// Inputs are observations, never worker presence or historical Ready events.
pub(crate) struct RecoveryObservation<'a> {
    pub diagnostic: Option<&'a str>,
    pub managed_resume_cause: Option<super::ManagedResumeCause>,
    pub operation_failed: bool,
    pub evidence: Option<super::OperationErrorEvidence>,
    pub runtime: super::RuntimeEvidence,
    pub work_in_progress: bool,
    pub output_uncertain: bool,
    pub owned_input: bool,
    pub owned_runtime: bool,
    pub same_qualified: bool,
    pub replacement_qualified: bool,
    pub channel_unavailable: bool,
}

impl SessionRecovery {
    pub(crate) fn project(
        row: &Instance,
        observation: RecoveryObservation<'_>,
        expected: String,
        mutations: bool,
    ) -> Option<Self> {
        use super::{ErrorBlockingScope, RuntimeEvidence};
        use SessionRecoveryReason::*;
        let diagnostic = observation
            .diagnostic
            .or(row.last_error.as_deref())
            .or_else(|| {
                observation
                    .managed_resume_cause
                    .and_then(|cause| cause.message().lines().next())
            });
        let evidence = observation.evidence.or(row.last_error_evidence);
        let exited = matches!(observation.runtime, RuntimeEvidence::PaneExited { .. });
        let failed_probe = row.resume_probe_failed_sid.is_some()
            && row.resume_probe_failed_sid == row.agent_session_id;
        let reason = if diagnostic.is_some()
            && (observation.operation_failed
                || evidence.is_some_and(|error| {
                    error.blocking_scope == Some(ErrorBlockingScope::Operation)
                })) {
            OperationFailed
        } else if observation.channel_unavailable {
            ChannelUnavailable
        } else if failed_probe {
            ProbeFailed
        } else if exited {
            RuntimeExited
        } else if (diagnostic.is_some() || row.status == Status::Error)
            && !observation.same_qualified
        {
            IdentityUnattested
        } else if diagnostic.is_some() || row.status == Status::Unknown {
            ObservationUnknown
        } else {
            return None;
        };
        let mutable = mutations
            && row.ensure_startable().is_ok()
            && row.terminal_handoff.is_none()
            && row.launch_is_finalized()
            && !row.has_fresh_lifecycle_reservation(Utc::now())
            && !matches!(
                row.status,
                Status::Creating | Status::Deleting | Status::Starting
            );
        let idle = !observation.work_in_progress;
        let actions = RecoveryActions {
            attach: !matches!(row.status, Status::Creating | Status::Deleting),
            retry_operation: mutable
                && idle
                && observation.owned_input
                && !observation.output_uncertain
                && evidence.is_some_and(|error| {
                    error.retryable == Some(true)
                        && error.blocking_scope == Some(ErrorBlockingScope::Operation)
                }),
            restart_same: mutable
                && idle
                && observation.same_qualified
                && observation.replacement_qualified,
            stop: mutable && observation.owned_runtime,
            fresh: mutable && idle && observation.replacement_qualified,
        };
        Some(Self {
            reason,
            diagnostic: diagnostic
                .map(crate::acp::acp_client::public_diagnostic)
                .unwrap_or_else(|| match reason {
                    RuntimeExited => "The observed runtime exited.".to_owned(),
                    ChannelUnavailable => "The agent channel is unavailable.".to_owned(),
                    ProbeFailed => "The selected conversation did not pass its resume probe.".to_owned(),
                    _ => "Current runtime or conversation authority is not established.".to_owned(),
                }),
            evidence,
            runtime: observation.runtime,
            work_in_progress: observation.work_in_progress,
            uncertainty: observation.output_uncertain.then(|| {
                "The previous input may have been accepted; its output could not be attributed. Do not resend it automatically.".to_owned()
            }),
            guidance: if observation.work_in_progress {
                "Agent work is still in progress. Observe it or explicitly stop it; no concurrent recovery operation is offered.".into()
            } else if let Some(cause) = observation.managed_resume_cause {
                cause.message().into()
            } else if !observation.same_qualified {
                "Observe the existing transcript and repair authentication or provider settings in the agent. Unknown identity grants no retry or Same shortcut. Fresh abandons pending AoE input and omits AoE restore flags; an undeclared wrapper may still restore its own history. Native and transcript history are retained.".into()
            } else {
                "Repair authentication or provider settings in the agent. Same preserves the qualified selection and sends no previous input. Fresh explicitly abandons pending AoE input, never replays prior input, and retains native and transcript history. Omitting AoE restore flags does not prevent an undeclared wrapper from restoring its own history.".into()
            },
            actions,
            expected,
        })
    }
}

/// Equality witness only. Never decode this as authority or expose private launch material.
pub(crate) fn recovery_selection(
    row: &Instance,
    storage: &super::Storage,
) -> anyhow::Result<String> {
    use sha2::{Digest, Sha256};
    let context = crate::acp::state::NativeEventContext::capture(row, storage)?;
    struct SelectionDigest(Sha256);
    impl std::io::Write for SelectionDigest {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.update(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut digest = SelectionDigest(Sha256::new());
    digest.0.update(b"aoe-session-recovery-selection-v1\0");
    digest.0.update(row.native_preparation_selection()?);
    serde_json::to_writer(
        &mut digest,
        &(
            context,
            (
                &row.acp_session_id,
                &row.fork_pending,
                row.import_pending,
                row.view,
                &row.project_path,
                &row.agent_name,
                &row.agent_provider,
                &row.acp_establishment,
            ),
            (
                &row.active_execution,
                &row.resume_probe_failed_sid,
                &row.omp_capture_generation,
                row.capture_started_at,
                row.started_at,
                row.force_fresh_next_launch,
                &row.sandbox_info,
                &row.terminal_handoff,
                &row.pending_initial_turn,
                &row.queued_prompts,
            ),
        ),
    )?;
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let bytes = digest.0.finalize();
    let mut token = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        token.push(HEX[(byte >> 4) as usize] as char);
        token.push(HEX[(byte & 0xF) as usize] as char);
    }
    Ok(token)
}

pub(crate) fn terminal_recovery_observation(row: &Instance) -> RecoveryObservation<'_> {
    use super::{PanePresence, RuntimeEvidence};
    let owned = row.agent_pane.state == PanePresence::Alive
        && row.agent_pane.physical.is_some()
        && row.agent_pane.legacy_tool.is_none();
    let runtime = match row.agent_pane.state {
        PanePresence::Alive if owned => RuntimeEvidence::PaneAlive,
        PanePresence::Dead | PanePresence::Absent => {
            RuntimeEvidence::PaneExited { exit_code: None }
        }
        _ => RuntimeEvidence::Unknown,
    };
    let same_qualified = row.clone().qualify_recovery_same().is_ok();
    // Unbound selections can fail qualification before command validation.
    let managed_resume_cause = if !same_qualified
        && matches!(row.status, Status::Error | Status::Unknown)
        && row.conversation_target().is_some()
    {
        crate::session::config::profile_config::resolve_config(&row.effective_profile())
            .and_then(|config| {
                Instance::execution_agent_for(&row.tool, row.get_tool_command(), &config.session)
            })
            .err()
            .and_then(|error| error.downcast_ref::<super::ManagedResumeCause>().copied())
            .filter(|cause| *cause == super::ManagedResumeCause::WrapperContractRequired)
    } else {
        None
    };
    RecoveryObservation {
        diagnostic: row.last_error.as_deref(),
        managed_resume_cause,
        operation_failed: false,
        evidence: row.last_error_evidence,
        runtime,
        work_in_progress: matches!(
            row.status,
            Status::Running | Status::Waiting | Status::Starting
        ),
        output_uncertain: false,
        owned_input: owned,
        owned_runtime: owned,
        same_qualified,
        replacement_qualified: owned || matches!(runtime, RuntimeEvidence::PaneExited { .. }),
        channel_unavailable: false,
    }
}

pub(crate) fn current_recovery(
    row: &Instance,
    storage: &super::Storage,
    folded: Option<&crate::acp::state::AcpState>,
    native: crate::acp::state::NativeAvailabilityProjection,
    mutations: bool,
) -> anyhow::Result<Option<SessionRecovery>> {
    use super::RuntimeEvidence;
    use crate::acp::establishment::AcpEstablishment;
    let expected = recovery_selection(row, storage)?;
    if !row.is_structured() {
        return Ok(SessionRecovery::project(
            row,
            terminal_recovery_observation(row),
            expected,
            mutations,
        ));
    }
    let context = crate::acp::state::NativeEventContext::capture(row, storage)?;
    let current = native.context() == Some(context);
    let available = current
        && (native.ready
            || native
                .unavailable
                .is_some_and(|unavailable| unavailable.source.is_channel_loss()));
    let pending = matches!(
        row.acp_establishment,
        Some(AcpEstablishment::Pending { .. })
    );
    let same_qualified = available
        && !pending
        && row.pending_initial_turn.is_none()
        && row.queued_prompts.is_empty()
        && row.check_acp_recovery_same(storage).is_ok();
    let observation = RecoveryObservation {
        managed_resume_cause: None,
        operation_failed: folded.is_some_and(|state| state.operation_diagnostic.is_some()),
        diagnostic: folded.and_then(|state| {
            state
                .operation_diagnostic
                .as_deref()
                .or(state.startup_diagnostic.as_deref())
        }),
        evidence: folded.and_then(|state| state.operation_error),
        runtime: if current && native.ready {
            RuntimeEvidence::AcpTransportReady
        } else if available && native.unavailable.is_some() {
            RuntimeEvidence::AcpTransportClosed
        } else {
            RuntimeEvidence::Unknown
        },
        work_in_progress: folded.is_none_or(|state| {
            state.turn_active
                || state.cancelling
                || state.compacting
                || state.has_qualified_background_work()
                || !state.pending_approvals.is_empty()
                || !state.pending_elicitations.is_empty()
                || !state.original_decision_nonces.is_empty()
        }),
        output_uncertain: folded.is_some_and(|state| state.operation_output_uncertain),
        owned_input: current && native.ready && !pending,
        owned_runtime: available && !pending,
        same_qualified,
        replacement_qualified: available && !pending,
        channel_unavailable: available && native.unavailable.is_some(),
    };
    Ok(SessionRecovery::project(
        row,
        observation,
        expected,
        mutations,
    ))
}

pub(crate) struct SessionProjection<'a> {
    pub id: &'a str,
    pub title: &'a str,
    pub project_path: &'a str,
    pub group_path: &'a str,
    pub tool: &'a str,
    pub command: &'a str,
    /// Supplied by the caller: a store listing names its own profile, which can
    /// differ from the row's `source_profile`.
    pub profile: &'a str,
    pub status: Status,
    pub bucket: SessionBucket,
    pub created_at: DateTime<Utc>,
    pub last_accessed_at: Option<DateTime<Utc>>,
    pub idle_entered_at: Option<DateTime<Utc>>,
    pub last_error: Option<&'a str>,
    pub last_error_evidence: Option<super::OperationErrorEvidence>,
    pub recovery: Option<SessionRecovery>,
    pub archived_at: Option<DateTime<Utc>>,
    pub trashed_at: Option<DateTime<Utc>>,
    pub active_snoozed_until: Option<DateTime<Utc>>,
    pub pinned_at: Option<DateTime<Utc>>,
    pub agent_session_id: Option<&'a str>,
    pub parent_session_id: Option<&'a str>,
    pub worktree: Option<&'a WorktreeInfo>,
    pub workspace_repos: &'a [WorkspaceRepo],
}

impl<'a> SessionProjection<'a> {
    pub(crate) fn new(inst: &'a Instance, profile: &'a str) -> Self {
        Self {
            id: &inst.id,
            title: &inst.title,
            project_path: &inst.project_path,
            group_path: &inst.group_path,
            tool: &inst.tool,
            command: &inst.command,
            profile,
            status: inst.status,
            bucket: inst.effective_bucket(),
            created_at: inst.created_at,
            last_accessed_at: inst.last_accessed_at,
            idle_entered_at: inst.idle_entered_at,
            last_error: inst.last_error.as_deref(),
            last_error_evidence: inst.last_error_evidence,
            recovery: SessionRecovery::project(
                inst,
                RecoveryObservation {
                    managed_resume_cause: None,
                    diagnostic: inst.last_error.as_deref(),
                    operation_failed: false,
                    evidence: inst.last_error_evidence,
                    runtime: super::RuntimeEvidence::Unknown,
                    work_in_progress: matches!(
                        inst.status,
                        Status::Running | Status::Waiting | Status::Starting
                    ),
                    output_uncertain: false,
                    owned_input: false,
                    owned_runtime: false,
                    same_qualified: false,
                    replacement_qualified: false,
                    channel_unavailable: false,
                },
                String::new(),
                false,
            ),
            archived_at: inst.archived_at,
            trashed_at: inst.trashed_at,
            active_snoozed_until: inst.active_snoozed_until(),
            pinned_at: inst.pinned_at,
            agent_session_id: inst.agent_session_id.as_deref(),
            parent_session_id: inst.parent_session_id.as_deref(),
            worktree: inst.worktree_info.as_ref(),
            workspace_repos: inst.all_repos(),
        }
    }

    pub(crate) fn has_managed_worktree(&self) -> bool {
        self.worktree
            .is_some_and(|worktree| worktree.managed_by_aoe)
    }
}

#[cfg(test)]
mod recovery_tests {
    use super::*;
    use crate::session::{
        ErrorBlockingScope, ErrorEvidenceSource, ErrorOperation, OperationErrorEvidence,
        PanePresence, ResumeIntent, RuntimeEvidence, Storage,
    };

    #[test]
    #[serial_test::serial]
    fn missing_wrapper_contract_projects_canonical_repair_without_granting_actions() {
        let _home = crate::session::test_support::isolate_app_dir();
        let mut row = Instance::new("wrapper recovery", "/tmp/project");
        row.tool = "claude".into();
        row.command = "work-claude".into();
        row.status = Status::Error;
        row.agent_session_id = Some("retained-native-id".into());
        let wrapper = SessionRecovery::project(
            &row,
            terminal_recovery_observation(&row),
            "opaque".into(),
            true,
        )
        .unwrap();
        assert_eq!(wrapper.reason, SessionRecoveryReason::IdentityUnattested);
        assert_eq!(
            wrapper.guidance,
            crate::session::ManagedResumeCause::WrapperContractRequired.message()
        );
        assert!(!wrapper.actions.restart_same && !wrapper.actions.retry_operation);

        row.status = Status::Running;
        assert!(SessionRecovery::project(
            &row,
            terminal_recovery_observation(&row),
            "opaque".into(),
            true,
        )
        .is_none());
        row.status = Status::Error;
        row.command = "claude".into();
        let unqualified = SessionRecovery::project(
            &row,
            terminal_recovery_observation(&row),
            "opaque".into(),
            true,
        )
        .unwrap();
        assert_eq!(
            unqualified.reason,
            SessionRecoveryReason::IdentityUnattested
        );
        assert_ne!(unqualified.diagnostic, wrapper.diagnostic);
        assert_eq!(wrapper.actions, unqualified.actions);
    }

    #[test]
    fn unknown_observation_never_turns_retry_metadata_or_a_raw_id_into_permission() {
        let mut row = Instance::new("unknown recovery", "/tmp/project");
        row.agent_session_id = Some("raw-id".into());
        row.status = Status::Error;
        row.last_error = Some("operation failed".into());
        let mut evidence = OperationErrorEvidence::new(
            ErrorEvidenceSource::LocalPromptResult,
            ErrorOperation::Prompt,
        );
        evidence.blocking_scope = Some(ErrorBlockingScope::Operation);
        evidence.retryable = Some(true);
        row.last_error_evidence = Some(evidence);
        let projected = SessionRecovery::project(
            &row,
            terminal_recovery_observation(&row),
            "opaque".into(),
            true,
        )
        .unwrap();
        assert_eq!(projected.reason, SessionRecoveryReason::OperationFailed);
        assert_eq!(projected.runtime, RuntimeEvidence::Unknown);
        assert_eq!(projected.evidence, Some(evidence));
        assert!(projected.actions.attach);
        assert!(!projected.actions.retry_operation);
        assert!(!projected.actions.restart_same);
        assert!(!projected.actions.stop);
        assert!(!projected.actions.fresh);
        assert_eq!(row.agent_session_id.as_deref(), Some("raw-id"));
        assert_eq!(row.resume_intent, ResumeIntent::Default);
    }

    #[test]
    fn confirmed_terminal_exit_offers_explicit_fresh_without_inventing_same_or_stop_authority() {
        let mut row = Instance::new("exited recovery", "/tmp/project");
        row.status = Status::Stopped;
        row.agent_pane.state = PanePresence::Dead;
        row.agent_session_id = Some("unqualified-history".into());
        let recovery = SessionRecovery::project(
            &row,
            terminal_recovery_observation(&row),
            "opaque".into(),
            true,
        )
        .unwrap();
        assert_eq!(recovery.reason, SessionRecoveryReason::RuntimeExited);
        assert!(matches!(
            recovery.runtime,
            RuntimeEvidence::PaneExited { .. }
        ));
        assert!(recovery.actions.attach && recovery.actions.fresh);
        assert!(!recovery.actions.restart_same && !recovery.actions.retry_operation);
        assert!(!recovery.actions.stop);
        let readonly = SessionRecovery::project(
            &row,
            terminal_recovery_observation(&row),
            "opaque".into(),
            false,
        )
        .unwrap();
        assert!(readonly.actions.attach);
        assert!(!readonly.actions.fresh);
        assert_eq!(row.agent_session_id.as_deref(), Some("unqualified-history"));
    }

    #[test]
    #[serial_test::serial]
    fn selection_fences_same_id_binding_intent_capture_generation_and_physical_namespace() {
        let _home = crate::session::test_support::isolate_app_dir();
        let first = Storage::new_unwatched("first").unwrap();
        let second = Storage::new_unwatched("second").unwrap();
        let mut row = Instance::new("selection", "/tmp/project");
        row.source_profile = "first".into();
        row.agent_session_id = Some("same-id".into());
        let original = recovery_selection(&row, &first).unwrap();
        let mut changed = row.clone();
        changed.resume_intent = ResumeIntent::Use("same-id".into());
        assert_ne!(original, recovery_selection(&changed, &first).unwrap());
        changed = row.clone();
        changed.omp_capture_generation = Some("different-capture".into());
        assert_ne!(original, recovery_selection(&changed, &first).unwrap());
        changed = row.clone();
        changed.lifecycle_generation += 1;
        assert_ne!(original, recovery_selection(&changed, &first).unwrap());
        changed = row.clone();
        changed.agent_session_binding = Some(crate::session::ConversationBinding {
            session_id: "same-id".into(),
            execution: None,
            provenance: crate::session::ConversationProvenance::Asserted,
            transcript_path: Some("/different/transcript.jsonl".into()),
        });
        assert_ne!(original, recovery_selection(&changed, &first).unwrap());
        assert_ne!(original, recovery_selection(&row, &second).unwrap());
        assert_eq!(original, recovery_selection(&row, &first).unwrap());
        assert!(!original.contains("same-id"));
    }
}
