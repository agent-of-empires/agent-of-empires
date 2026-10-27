//! Rename rules shared by the daemon, the CLI and smart rename: refusals,
//! uniqueness, the can-move gate, the worktree effect and the tmux rekey
//! warning. Each surface keeps its own locking and persistence.

use std::fmt::Display;

use crate::session::worktree_edit::{
    discard_sandbox_container_after_move, edit_worktree_workdir, ensure_sandbox_container_released,
    WorktreeEditError, WorktreeEditOutcome, WorktreeEditRequest,
};
use crate::session::{duplicate_session_error, is_duplicate_session, Instance};

#[derive(Debug, thiserror::Error)]
pub enum RenameRefusal {
    #[error("{}", duplicate_session_error(.0))]
    Duplicate(String),
    #[error("Stop the session before renaming its worktree directory or branch")]
    Running,
    #[error(
        "The sandbox container still holds the worktree; stop it before renaming its directory"
    )]
    ContainerHoldsWorktree,
    #[error("Renaming is unified while Tie Worktree Directory to Session Name is on; rename the session instead")]
    Tied,
}

/// Refuse a title, profile or path change that collides with another row.
/// `rows` are the destination profile's rows; the session's own row is
/// excluded only for a same-profile edit.
pub fn ensure_unique(
    rows: &[Instance],
    before: &Instance,
    after: &Instance,
) -> Result<(), RenameRefusal> {
    let pair_changed = before.title != after.title
        || before.source_profile != after.source_profile
        || before.project_path.trim_end_matches('/') != after.project_path.trim_end_matches('/');
    if pair_changed
        && is_duplicate_session(
            rows.iter(),
            &after.title,
            &after.project_path,
            (before.source_profile == after.source_profile).then_some(before.id.as_str()),
        )
    {
        return Err(RenameRefusal::Duplicate(after.title.clone()));
    }
    Ok(())
}

/// [`ensure_unique`] for a title-only change.
pub fn ensure_unique_title(
    rows: &[Instance],
    session: &Instance,
    title: &str,
) -> Result<(), RenameRefusal> {
    let mut after = session.clone();
    after.title = title.to_owned();
    ensure_unique(rows, session, &after)
}

/// What an edit does to the worktree on disk.
#[derive(Clone, Copy)]
pub struct WorktreeChange {
    pub moves_dir: bool,
    pub renames_branch: bool,
}

/// An active session blocks a worktree change. `strict` also refuses a no-op
/// edit (standalone workdir edit).
pub fn ensure_idle(
    status_blocks: bool,
    change: WorktreeChange,
    strict: bool,
) -> Result<(), RenameRefusal> {
    if status_blocks && (strict || change.moves_dir || change.renames_branch) {
        return Err(RenameRefusal::Running);
    }
    Ok(())
}

/// A sandbox container pins the worktree bind mount, so a directory move needs
/// it released. Fails closed when its state cannot be read.
pub fn ensure_container_released(
    session_id: &str,
    sandboxed: bool,
    change: WorktreeChange,
) -> Result<(), RenameRefusal> {
    if change.moves_dir && ensure_sandbox_container_released(session_id, sandboxed) {
        return Err(RenameRefusal::ContainerHoldsWorktree);
    }
    Ok(())
}

/// The can-move decision: idle first, then the container.
pub fn worktree_edit_gate(
    session_id: &str,
    sandboxed: bool,
    status_blocks: bool,
    change: WorktreeChange,
    strict: bool,
) -> Result<(), RenameRefusal> {
    ensure_idle(status_blocks, change, strict)?;
    ensure_container_released(session_id, sandboxed, change)
}

/// Apply the git edit, then drop the now-stale sandbox container if the
/// directory moved. The gate must already have passed.
pub fn apply_worktree_edit(
    request: WorktreeEditRequest<'_>,
    session_id: &str,
    sandboxed: bool,
) -> Result<WorktreeEditOutcome, WorktreeEditError> {
    let current_path = request.current_path;
    let outcome = edit_worktree_workdir(request)?;
    if outcome.new_path != current_path {
        discard_sandbox_container_after_move(session_id, sandboxed);
    }
    Ok(outcome)
}

pub fn rekey_warning(error: &dyn Display) -> String {
    format!("Session metadata was renamed, but its live tmux session could not be rekeyed: {error}")
}

/// Rekey the live tmux session after the new title is committed. `Some` is a
/// warning for the surface to render.
pub fn rekey_tmux(id: &str, old_title: &str, new_title: &str) -> Option<String> {
    rekey_with(|| crate::tmux::rekey_session(id, old_title, new_title))
}

fn rekey_with<T>(rekey: impl FnOnce() -> anyhow::Result<T>) -> Option<String> {
    rekey().err().map(|error| rekey_warning(&error))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MOVE: WorktreeChange = WorktreeChange {
        moves_dir: true,
        renames_branch: false,
    };
    const BRANCH: WorktreeChange = WorktreeChange {
        moves_dir: false,
        renames_branch: true,
    };
    const NOOP: WorktreeChange = WorktreeChange {
        moves_dir: false,
        renames_branch: false,
    };

    #[test]
    fn idle_gate_blocks_active_sessions_by_change_and_strictness() {
        for (status_blocks, change, strict, blocked) in [
            (true, MOVE, false, true),
            (true, BRANCH, false, true),
            (true, NOOP, false, false),
            (true, NOOP, true, true),
            (false, MOVE, true, false),
        ] {
            assert_eq!(
                ensure_idle(status_blocks, change, strict).is_err(),
                blocked,
                "blocks={status_blocks} moves={} branch={} strict={strict}",
                change.moves_dir,
                change.renames_branch,
            );
        }
        assert!(matches!(
            worktree_edit_gate("id", false, true, MOVE, false),
            Err(RenameRefusal::Running)
        ));
        assert!(worktree_edit_gate("id", false, false, MOVE, false).is_ok());
    }

    #[test]
    fn uniqueness_covers_title_path_and_profile() {
        let mut other = Instance::new("taken", "/tmp/repo/");
        other.source_profile = "default".into();
        let mut before = Instance::new("mine", "/tmp/repo");
        before.source_profile = "default".into();
        let rows = vec![other, before.clone()];

        let renamed = |title: &str, path: &str, profile: &str| {
            let mut after = before.clone();
            after.title = title.into();
            after.project_path = path.into();
            after.source_profile = profile.into();
            after
        };
        for (after, duplicate) in [
            (renamed("taken", "/tmp/repo", "default"), true),
            (renamed("taken", "/tmp/elsewhere", "default"), false),
            (renamed("mine", "/tmp/repo/", "default"), false),
            // Cross-profile moves keep the row's own id in play.
            (renamed("mine", "/tmp/repo", "other"), true),
        ] {
            let result = ensure_unique(&rows, &before, &after);
            assert_eq!(
                matches!(result, Err(RenameRefusal::Duplicate(_))),
                duplicate,
                "{} {} {}",
                after.title,
                after.project_path,
                after.source_profile,
            );
        }
    }

    #[test]
    fn rekey_failure_yields_one_warning() {
        assert_eq!(rekey_with(|| Ok(true)), None);
        let warning = rekey_with::<bool>(|| anyhow::bail!("no server")).unwrap();
        assert_eq!(warning, rekey_warning(&"no server"));
        assert!(warning.ends_with("rekeyed: no server"));
    }
}
