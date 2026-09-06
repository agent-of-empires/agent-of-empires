//! The `SessionResponse` wire model and per-request config resolution.

use super::*;

impl SessionResponse {
    /// Build a response from a session instance plus the user's current
    /// Claude Code fullscreen-renderer preference.
    ///
    /// `claude_fullscreen` is the *user-level* setting (read once per
    /// request via `crate::claude_settings::read_tui_fullscreen()`); it
    /// surfaces on the response only when the session's agent is Claude.
    pub fn from_instance(inst: &Instance, claude_fullscreen: bool) -> Self {
        Self::from_instance_with_plan(
            inst,
            claude_fullscreen,
            None,
            crate::daemon::AcpWorkerState::Absent,
            None,
            None,
            None,
        )
    }

    /// Build a response with the per-session plan snapshot. Called from
    /// the REST sessions endpoint after a single bulk read of the
    /// structured view event store; see #1061.
    pub fn from_instance_with_plan(
        inst: &Instance,
        claude_fullscreen: bool,
        plan_summary: Option<PlanSummary>,
        acp_worker_state: crate::daemon::AcpWorkerState,
        next_wakeup_at: Option<String>,
        next_wakeup_reason: Option<String>,
        // `Some(description)` when the session has an armed `Monitor` (the
        // inner description is itself optional); `None` when none is armed.
        // Mirrors `EventStore::latest_active_monitor`'s return so the caller
        // forwards it verbatim.
        active_monitor: Option<Option<String>>,
    ) -> Self {
        let (monitor_active, monitor_description) = match active_monitor {
            Some(description) => (true, description),
            None => (false, None),
        };
        Self {
            id: inst.id.clone(),
            title: inst.title.clone(),
            project_path: inst.project_path.clone(),
            artifact_dir: crate::session::artifacts::artifact_dir_path(&inst.id)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default(),
            group_path: inst.group_path.clone(),
            tool: inst.tool.clone(),
            status: inst.status.wire_str().to_string(),
            dormant: inst.is_shown_dormant(),
            yolo_mode: inst.yolo_mode,
            created_at: inst.created_at.to_rfc3339(),
            last_accessed_at: inst.last_accessed_at.map(|t| t.to_rfc3339()),
            idle_entered_at: inst.idle_entered_at.map(|t| t.to_rfc3339()),
            last_error: inst.last_error.clone(),
            branch: inst.worktree_info.as_ref().map(|w| w.branch.clone()),
            main_repo_path: inst
                .worktree_info
                .as_ref()
                .map(|w| w.main_repo_path.clone()),
            base_branch: inst
                .worktree_info
                .as_ref()
                .and_then(|w| w.base_branch.clone()),
            base_branch_override: inst.base_branch_override.clone(),
            is_sandboxed: inst.is_sandboxed(),
            scratch: inst.scratch,
            favorited: inst.is_favorited(),
            color: inst.color.clone(),
            urgent: inst.is_urgent(),
            pinned_at: inst.pinned_at.map(|t| t.to_rfc3339()),
            archived_at: inst.archived_at.map(|t| t.to_rfc3339()),
            // Surface `snoozed_until` only when the snooze is still
            // active. `is_snoozed()` returns false once the timestamp
            // has expired, even though the persisted field stays set
            // until the next mutation rewrites it. Mirroring that
            // semantics on the wire prevents the web sidebar from
            // showing a "snoozed 0m" chip on rows that have already
            // woken on disk.
            snoozed_until: if inst.is_snoozed() {
                inst.snoozed_until.map(|t| t.to_rfc3339())
            } else {
                None
            },
            trashed_at: inst.trashed_at.map(|t| t.to_rfc3339()),
            // Surface the marker (omitted when read); the web gates the
            // visual on the `session.unread_indicator` setting.
            unread: inst.unread,
            has_managed_worktree: inst
                .worktree_info
                .as_ref()
                .is_some_and(|w| w.managed_by_aoe),
            has_cleanable_worktree: inst.has_managed_worktree_or_workspace(),
            // Overlaid per-profile in list_sessions; see the field doc.
            tie_workdir_to_name: false,
            // Overlaid in list_sessions; single-session responses stay inactive.
            smart_rename: crate::session::smart_rename::SmartRenameState::Inactive,
            // Overlaid in list_sessions; single-session responses stay false.
            default_name: false,
            has_terminal: inst.terminal_info.is_some(),
            profile: inst.source_profile.clone(),
            cleanup_defaults: CleanupDefaults {
                delete_worktree: true,
                delete_branch: false,
                delete_sandbox: true,
                delete_to_trash: true,
            },
            remote_owner: None,
            remote_owner_key: None,
            notify_on_waiting: inst.notify_on_waiting,
            notify_on_idle: inst.notify_on_idle,
            notify_on_error: inst.notify_on_error,
            view: inst.view,
            context_resume: Some(context_resume_for(inst)),
            queued_prompts: {
                let mut q = inst.queued_prompts.clone();
                q.sort_by_key(|e| e.seq);
                q
            },
            acp_worker_state,
            // Built-in ACP capability is resolved here from a process-wide
            // registry (cheap, no IO). Custom agents depend on profile
            // config; the list and create handlers overlay that without a
            // per-row config read.
            acp_capable: {
                let resolved = inst
                    .agent_name
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .unwrap_or(inst.tool.as_str());
                builtin_acp_registry().get(resolved).is_some()
            },
            acp_session_id: inst.acp_session_id.clone(),
            // Resolved the same way as `acp_capable` above: `agent_name` when
            // set and non-empty, else `tool`. This is the ACP registry key,
            // so it matches `/api/acp/agents` names the switch-agent modal
            // filters against. See #2803.
            acp_agent: {
                let resolved = inst
                    .agent_name
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .unwrap_or(inst.tool.as_str());
                (!resolved.is_empty()).then(|| resolved.to_string())
            },
            // The create-time guard calls the same classifier, so the web
            // "Fork" affordance and server-side acceptance cannot drift.
            acp_can_fork: agent_is_structured_fork_capable(&inst.tool, inst.agent_name.as_deref()),
            // Same agent resolution as `acp_agent` above; computed once here so
            // the web dashboard and native TUI stop mirroring the gate.
            keeps_context: crate::agents::acp_transcript_cli_resumable(
                &inst.tool,
                inst.agent_name
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .unwrap_or(inst.tool.as_str()),
            ),
            // Same agent resolution as `acp_agent` above; the composer palette
            // and queued-prompt clear-boundary hint read these instead of a
            // client-side per-agent mirror.
            clear_aliases: crate::acp::agent_profiles::resolve(
                inst.agent_name
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .unwrap_or(inst.tool.as_str()),
            )
            .clear_aliases
            .iter()
            .map(|s| s.to_string())
            .collect(),
            claude_fullscreen: claude_fullscreen && inst.tool == "claude",
            // A session converted by `attach_project` (#3103) has a real
            // `workspace_info`, so this lists both repos with no special case:
            // the structured view's repo-relative path rendering, the diff-repo
            // resolver and the sidebar's multi-repo grouping all see the same
            // shape they see for a session created multi-repo.
            workspace_repos: inst
                .all_repos()
                .iter()
                .map(|r| WorkspaceRepoSummary {
                    name: r.name.clone(),
                    source_path: r.source_path.clone(),
                    branch: r.branch.clone(),
                })
                .collect(),
            warnings: Vec::new(),
            plan_summary,
            next_wakeup_at,
            next_wakeup_reason,
            monitor_active,
            monitor_description,
        }
    }
}

/// Project a stored `Plan` into the lightweight `PlanSummary` shape the
/// sidebar consumes. Current step is the first non-Done entry; counts
/// reflect the persisted step state from the agent's last PlanUpdated.
pub(super) fn plan_summary_from_plan(plan: crate::acp::state::Plan) -> PlanSummary {
    use crate::acp::state::PlanStepStatus;
    let total = plan.steps.len() as u32;
    let completed = plan
        .steps
        .iter()
        .filter(|s| matches!(s.status, PlanStepStatus::Done))
        .count() as u32;
    let current_step_title = plan
        .steps
        .iter()
        .find(|s| !matches!(s.status, PlanStepStatus::Done))
        .map(|s| truncate_title(&s.title, 80));
    PlanSummary {
        current_step_title,
        completed,
        total,
    }
}

pub(super) fn truncate_title(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

pub(super) fn context_resume_for(inst: &Instance) -> ContextResumeAvailability {
    if inst.is_structured() {
        return if inst.fork_pending.is_some() {
            ContextResumeAvailability::Unavailable {
                reason: ContextResumeUnavailableReason::ForkPending,
            }
        } else if inst.acp_session_id.is_none() {
            ContextResumeAvailability::Unavailable {
                reason: ContextResumeUnavailableReason::NoTarget,
            }
        } else {
            match inst.acp_load_session_capable {
                Some(true) => ContextResumeAvailability::Available,
                Some(false) => ContextResumeAvailability::Unavailable {
                    reason: ContextResumeUnavailableReason::AgentUnsupported,
                },
                None => ContextResumeAvailability::Indeterminate {
                    reason: ContextResumeIndeterminateReason::AgentHandshakeRequired,
                },
            }
        };
    }

    match inst.terminal_context_resume_cached() {
        TerminalContextResume::Available => ContextResumeAvailability::Available,
        TerminalContextResume::RuntimeCheckRequired => ContextResumeAvailability::Indeterminate {
            reason: ContextResumeIndeterminateReason::RuntimeCheckRequired,
        },
        TerminalContextResume::NoTarget => ContextResumeAvailability::Unavailable {
            reason: ContextResumeUnavailableReason::NoTarget,
        },
        TerminalContextResume::AgentUnsupported => ContextResumeAvailability::Unavailable {
            reason: ContextResumeUnavailableReason::AgentUnsupported,
        },
        TerminalContextResume::SandboxUnsupported => ContextResumeAvailability::Unavailable {
            reason: ContextResumeUnavailableReason::SandboxUnsupported,
        },
        TerminalContextResume::CommandUnsupported => ContextResumeAvailability::Unavailable {
            reason: ContextResumeUnavailableReason::CommandUnsupported,
        },
        TerminalContextResume::ForcedFresh => ContextResumeAvailability::Unavailable {
            reason: ContextResumeUnavailableReason::ForcedFresh,
        },
        TerminalContextResume::InvalidTarget => ContextResumeAvailability::Unavailable {
            reason: ContextResumeUnavailableReason::InvalidTarget,
        },
        TerminalContextResume::ForkPending => ContextResumeAvailability::Unavailable {
            reason: ContextResumeUnavailableReason::ForkPending,
        },
        TerminalContextResume::PreviousFailure => ContextResumeAvailability::Unavailable {
            reason: ContextResumeUnavailableReason::PreviousFailure,
        },
    }
}

/// Process-wide built-in ACP registry, built once. Used to compute
/// `SessionResponse.acp_capable` for built-in agents without allocating
/// a registry per response row.
fn builtin_acp_registry() -> &'static crate::acp::AgentRegistry {
    static REG: std::sync::OnceLock<crate::acp::AgentRegistry> = std::sync::OnceLock::new();
    REG.get_or_init(crate::acp::AgentRegistry::with_defaults)
}

/// True iff this custom agent can run in structured view: it declares a valid
/// `agent_acp_cmd`, or it inherits a registry-backed base via
/// `agent_detect_as`. Built-in capability is handled separately in the
/// constructor, so this only covers the custom case.
pub(super) fn custom_agent_acp_capable(
    session: &crate::session::config::SessionConfig,
    tool: &str,
) -> bool {
    session
        .agent_acp_cmd
        .get(tool)
        .is_some_and(|cmd| crate::acp::AgentSpec::from_acp_cmd(tool, cmd).is_ok())
        || crate::acp::inherited_acp_base(tool, &session.agent_detect_as).is_some()
}

/// Resolve the [`SessionConfig`] for `(profile, project_path)` through the
/// caller-owned per-request cache, resolving from disk on first miss only.
/// See the `session_cfg_cache` declaration in `list_sessions` for the
/// sharing rationale. See #2603.
pub(super) fn resolve_session_cfg<'a>(
    cache: &'a mut HashMap<(String, String), SessionConfig>,
    profile: &str,
    project_path: &str,
) -> &'a SessionConfig {
    cache
        .entry((profile.to_string(), project_path.to_string()))
        .or_insert_with(|| {
            #[cfg(test)]
            LIST_SESSIONS_RESOLVER_MISSES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            crate::session::config::repo_config::resolve_config_with_repo_or_warn(
                profile,
                std::path::Path::new(project_path),
            )
            .session
        })
}

/// Test seam for the shared per-request cache invariant (#2603): bumped
/// exactly once per unique `(profile, project_path)` that resolves through
/// [`resolve_session_cfg`]. Mirrors the module-static test seam pattern used
/// by [`crate::session::FAIL_NEXT_LIST_PROFILES`]. Readers must hold
/// `#[serial_test::serial]`: a concurrent `list_sessions` call between reset
/// and load would leak bumps into the assertion.
#[cfg(test)]
pub(crate) static LIST_SESSIONS_RESOLVER_MISSES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
