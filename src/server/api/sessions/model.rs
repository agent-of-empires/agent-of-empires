//! The `SessionResponse` wire model and per-request config resolution.

use super::*;
use crate::session::config::StructuredView;
use crate::session::projection::SessionProjection;
use chrono::{DateTime, Utc};

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
        // `Some(description)` when the session has an armed `Monitor`, `None`
        // otherwise, mirroring `EventStore::latest_active_monitor`.
        active_monitor: Option<Option<String>>,
    ) -> Self {
        let (monitor_active, monitor_description) = match active_monitor {
            Some(description) => (true, description),
            None => (false, None),
        };
        let row = SessionProjection::new(inst, &inst.source_profile);
        let timestamp = |value: Option<DateTime<Utc>>| value.map(|t| t.to_rfc3339());
        let status = if inst
            .lifecycle_reservation
            .as_ref()
            .is_some_and(|reservation| reservation.op == crate::session::LifecycleOperation::Purge)
        {
            Status::Deleting
        } else {
            row.status
        };
        Self {
            agent_pane: inst.agent_pane.clone(),
            auxiliary: inst.auxiliary.clone(),
            id: row.id.to_string(),
            idempotency_key: inst.idempotency_key.clone(),
            title: row.title.to_string(),
            project_path: row.project_path.to_string(),
            agent_session_id: inst.agent_session_id.clone(),
            parent_session_id: row.parent_session_id.map(str::to_string),
            artifact_dir: crate::session::artifacts::artifact_dir_path(&inst.id)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default(),
            group_path: row.group_path.to_string(),
            sort_index: inst.sort_index,
            tool: row.tool.to_string(),
            command: row.command.to_string(),
            extra_args: inst.extra_args.clone(),
            status: status.wire_str().to_string(),
            lifecycle_reservation: inst.lifecycle_reservation.clone(),
            lifecycle_generation: inst.lifecycle_generation,
            dormant: inst.is_shown_dormant(),
            idle_dormant_since: inst.idle_dormant_since.map(|t| t.to_rfc3339()),
            pane_dead_observed: inst.pane_dead_observed,
            yolo_mode: inst.yolo_mode,
            created_at: row.created_at.to_rfc3339(),
            last_accessed_at: timestamp(row.last_accessed_at),
            idle_entered_at: timestamp(row.idle_entered_at),
            last_error: row.last_error.map(str::to_string),
            branch: row.worktree.map(|w| w.branch.clone()),
            main_repo_path: row.worktree.map(|w| w.main_repo_path.clone()),
            base_branch: row.worktree.and_then(|w| w.base_branch.clone()),
            base_branch_override: inst.base_branch_override.clone(),
            worktree_created_at: inst
                .worktree_info
                .as_ref()
                .map(|w| w.created_at.to_rfc3339()),
            is_sandboxed: inst.is_sandboxed(),
            sandbox_container_name: inst.sandbox_info.as_ref().map(|s| s.container_name.clone()),
            // Worktree and workspace creation refuse a path that is not a Repository.
            outside_repository: inst.worktree_info.is_none()
                && inst.workspace_info.is_none()
                && outside_repository_cached(&inst.project_path),
            scratch: inst.scratch,
            favorited: inst.is_favorited(),
            favorited_at: inst.favorited_at.map(|t| t.to_rfc3339()),
            color: inst.color.clone(),
            urgent: inst.is_urgent(),
            pinned_at: timestamp(row.pinned_at),
            archived_at: timestamp(row.archived_at),
            snoozed_until: timestamp(row.active_snoozed_until),
            trashed_at: timestamp(row.trashed_at),
            trash_lifecycle_id: inst.trash_lifecycle_id.clone(),
            failed_purge_forced: inst.failed_purge_forced(),
            // Surface the marker; the web gates the visual on the
            // `session.unread_indicator` setting.
            unread: inst.unread,
            has_managed_worktree: row.has_managed_worktree(),
            has_cleanable_worktree: inst.has_managed_worktree_or_workspace(),
            // Overlaid per-profile in list_sessions; see the field doc.
            tie_workdir_to_name: false,
            // Overlaid in list_sessions; single-session responses stay inactive.
            smart_rename: crate::session::smart_rename::SmartRenameState::Inactive,
            // Overlaid in list_sessions; single-session responses stay false.
            default_name: false,
            has_terminal: inst.terminal_info.is_some(),
            profile: row.profile.to_string(),
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
            acp_worker_state,
            acp_worker_start: None,
            update_restart: false,
            pending_approvals: Vec::new(),
            rate_limit: None,
            rate_limit_auto_resume: None,
            // Built-in ACP capability resolves here from a process-wide
            // registry (no IO). Custom agents depend on profile config, which
            // the list and create handlers overlay without a per-row read.
            acp_capable: {
                let resolved = inst
                    .agent_name
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .unwrap_or(inst.tool.as_str());
                builtin_acp_registry().get(resolved).is_some()
            },
            acp_session_id: inst.acp_session_id.clone(),
            // Resolved like `acp_capable`: `agent_name` when non-empty, else
            // `tool`. This is the ACP registry key, so it matches the
            // `/api/acp/agents` names the switch-agent modal filters on (#2803).
            acp_agent: {
                let resolved = inst
                    .agent_name
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .unwrap_or(inst.tool.as_str());
                (!resolved.is_empty()).then(|| resolved.to_string())
            },
            acp_provider: inst.agent_provider.clone(),
            // The create-time guard calls the same classifier, so the web
            // "Fork" affordance and server-side acceptance cannot drift.
            acp_can_fork: crate::session::fork::structured_fork_capable(
                &inst.tool,
                inst.agent_name.as_deref(),
            ),
            structured_view_off: false,
            // Same agent resolution as `acp_agent` above; computed once here so
            // the web dashboard and native TUI stop mirroring the gate.
            keeps_context: crate::agents::acp_transcript_cli_resumable(
                &inst.tool,
                inst.agent_name
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .unwrap_or(inst.tool.as_str()),
            ),
            // Same agent resolution as `acp_agent`; the composer palette and
            // queued-prompt clear-boundary hint read these instead of a
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
            // `workspace_info`, so both repos list here with no special case.
            workspace_repos: row
                .workspace_repos
                .iter()
                .map(|r| WorkspaceRepoSummary {
                    name: r.name.clone(),
                    source_path: r.source_path.clone(),
                    branch: r.branch.clone(),
                    worktree_path: r.worktree_path.clone(),
                    main_repo_path: r.main_repo_path.clone(),
                    managed_by_aoe: r.managed_by_aoe,
                    branch_preexisting: r.branch_preexisting,
                    base_branch: r.base_branch.clone(),
                    base_branch_override: r.base_branch_override.clone(),
                })
                .collect(),
            workspace_dir: inst
                .workspace_info
                .as_ref()
                .map(|w| w.workspace_dir.clone()),
            workspace_branch: inst.workspace_info.as_ref().map(|w| w.branch.clone()),
            workspace_created_at: inst
                .workspace_info
                .as_ref()
                .map(|w| w.created_at.to_rfc3339()),
            workspace_cleanup_on_delete: inst.workspace_info.as_ref().map(|w| w.cleanup_on_delete),
            warnings: Vec::new(),
            plan_summary,
            next_wakeup_at,
            next_wakeup_reason,
            monitor_active,
            monitor_description,
        }
    }
}

/// Clients poll the Session list every few seconds, so a probe is reused
/// briefly instead of opening every Working directory on each poll.
fn outside_repository_cached(path: &str) -> bool {
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};
    const TTL: Duration = Duration::from_secs(30);
    static CACHE: OnceLock<Mutex<HashMap<String, (Instant, bool)>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);

    if let Some((at, outside)) = cache.lock().ok().and_then(|c| c.get(path).copied()) {
        if at.elapsed() < TTL {
            return outside;
        }
    }
    let outside = crate::git::is_outside_repository(std::path::Path::new(path));
    if let Ok(mut c) = cache.lock() {
        // Sweep on write so paths of deleted Sessions cannot accumulate.
        c.retain(|_, (at, _)| at.elapsed() < TTL);
        c.insert(path.to_string(), (Instant::now(), outside));
    }
    outside
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

/// A plain session keeps `RuntimeCheckRequired` until a resume probe runs, so a captured
/// agent session id is the evidence that a restart keeps the conversation.
pub(super) fn update_restart_resumable(
    inst: &Instance,
    resume: &ContextResumeAvailability,
) -> bool {
    match resume {
        ContextResumeAvailability::Available => true,
        ContextResumeAvailability::Indeterminate {
            reason: ContextResumeIndeterminateReason::RuntimeCheckRequired,
        } => inst.agent_session_id.is_some(),
        _ => false,
    }
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

/// Per-request cache for `(profile, project_path)` config resolution, shared
/// across the `list_sessions` overlays so a repo-local override is read from
/// disk once per unique pair rather than once per row. See #2603.
pub(super) struct SessionCfgCache<'a> {
    entries: HashMap<(String, String), (SessionConfig, StructuredView)>,
    /// Where this cache reports its disk reads. The counter belongs to the
    /// request, not to the cache, so every cache the request opens adds to
    /// one total: splitting the shared cache per overlay then shows up as
    /// extra resolutions instead of hiding behind a second private tally.
    /// There is no counter-less constructor for the same reason.
    misses: &'a std::sync::atomic::AtomicUsize,
}

impl<'a> SessionCfgCache<'a> {
    pub(super) fn new(misses: &'a std::sync::atomic::AtomicUsize) -> Self {
        Self {
            entries: HashMap::new(),
            misses,
        }
    }
    /// Resolve `(profile, project_path)`, reading from disk on first miss only.
    fn entry(&mut self, profile: &str, project_path: &str) -> &(SessionConfig, StructuredView) {
        let misses = self.misses;
        self.entries
            .entry((profile.to_string(), project_path.to_string()))
            .or_insert_with(|| {
                misses.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let config = crate::session::config::repo_config::resolve_config_with_repo_or_warn(
                    profile,
                    std::path::Path::new(project_path),
                );
                (config.session, config.acp.structured_view)
            })
    }

    pub(super) fn resolve(&mut self, profile: &str, project_path: &str) -> &SessionConfig {
        &self.entry(profile, project_path).0
    }

    pub(super) fn structured_view(&mut self, profile: &str, project_path: &str) -> StructuredView {
        self.entry(profile, project_path).1
    }
}

/// Per-request cache of each profile's merged project registry, keyed by canonical path, so a
/// per-project override lookup (e.g. `smart_rename`) reads and canonicalizes the registry once per
/// profile per request rather than once per session row.
pub(super) struct ProjectRegistryCache {
    by_profile: HashMap<String, Vec<(String, crate::session::Project)>>,
}

impl ProjectRegistryCache {
    pub(super) fn new() -> Self {
        Self {
            by_profile: HashMap::new(),
        }
    }

    fn find(&mut self, profile: &str, project_path: &str) -> Option<&crate::session::Project> {
        use crate::session::projects::{canonical_key, load_merged};
        let projects = self
            .by_profile
            .entry(profile.to_string())
            .or_insert_with(|| {
                load_merged(profile)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|p| (canonical_key(&p.path), p))
                    .collect()
            });
        let target = canonical_key(project_path);
        projects
            .iter()
            .find(|(key, _)| *key == target)
            .map(|(_, p)| p)
    }

    /// Scratch sessions have no stable path to key a registry entry on, so their override lives
    /// on `session_cfg` (already resolved by the caller) instead of the project registry.
    pub(super) fn smart_rename_override(
        &mut self,
        profile: &str,
        scratch: bool,
        project_path: &str,
        session_cfg: &crate::session::config::SessionConfig,
    ) -> Option<bool> {
        if scratch {
            return session_cfg.scratch_smart_rename.as_override();
        }
        self.find(profile, project_path)
            .and_then(|p| p.overrides.smart_rename)
    }
}
