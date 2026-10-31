//! The session facts every read surface (CLI, runtime wire, REST) shares.
//!
//! Borrowed and typed so each surface formats timestamps and names fields to
//! its own contract without a second derivation from the `Instance`.

use chrono::{DateTime, Utc};

use super::{Instance, SessionBucket, Status, WorkspaceRepo, WorktreeInfo};

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
