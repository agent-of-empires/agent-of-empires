//! `agent-of-empires remove` command implementation

use anyhow::Result;
use clap::Args;

use crate::session::{Instance, Storage};

#[derive(Args)]
pub struct RemoveArgs {
    /// Session ID or title to remove
    identifier: String,

    /// Delete worktree directory (default: keep worktree)
    #[arg(long = "delete-worktree")]
    delete_worktree: bool,

    /// Delete git branch after worktree removal (default: per config)
    #[arg(long = "delete-branch")]
    delete_branch: bool,

    /// Force worktree removal even with untracked/modified files
    #[arg(long)]
    force: bool,

    /// Keep container instead of deleting it (default: delete per config)
    #[arg(long = "keep-container")]
    keep_container: bool,

    /// For scratch sessions, keep the scratch directory on disk instead of
    /// removing it. The session record is still deleted; the kept path is
    /// logged so you can find the files later. No effect on non-scratch
    /// sessions.
    #[arg(long = "keep-scratch")]
    keep_scratch: bool,

    /// Permanently delete instead of moving to trash. By default `rm` moves
    /// the session to the trash (when `session.delete_to_trash` is enabled,
    /// the default) so it can be restored; `--purge` forces the irreversible
    /// teardown (worktree/branch/container cleanup per the other flags, plus
    /// transcript removal).
    #[arg(long)]
    purge: bool,
}

fn needs_worktree_cleanup(inst: &Instance, args: &RemoveArgs) -> bool {
    args.delete_worktree && inst.has_managed_worktree_or_workspace()
}

#[tracing::instrument(target = "cli.session", skip_all, fields(profile = %profile))]
pub async fn run(profile: &str, args: RemoveArgs) -> Result<()> {
    let storage = Storage::new_unwatched(profile)?;

    // Phase 1 (unlocked): identify the target and run the slow deletion
    // side effects (worktree removal, branch deletion, container teardown,
    // detach hooks). The flock would otherwise be held for the entire
    // deletion sequence, blocking peer mutators on the same profile.
    let (instances, _groups) = storage.load_with_groups()?;

    let inst = super::resolve_session(&args.identifier, &instances)
        .map_err(|e| anyhow::anyhow!("{} in profile '{}'", e, storage.profile()))?
        .clone();
    let removed_id = inst.id.clone();
    let removed_title = inst.title.clone();

    let config = crate::session::repo_config::resolve_config_with_repo_or_warn(
        profile,
        std::path::Path::new(&inst.project_path),
    );

    // Trash-first: unless --purge is given (or delete_to_trash is disabled),
    // stop the live session and mark it trashed, keeping every durable
    // artifact so it can be restored. Mirrors the archive CLI's tmux
    // teardown. See #2489.
    if config.session.delete_to_trash && !args.purge {
        if let Err(e) = inst.kill() {
            eprintln!("Warning: failed to kill agent tmux session: {}", e);
        }
        inst.kill_ancillary_tmux_sessions();

        let landed = storage.update(|all_instances, _groups| {
            if let Some(stored) = all_instances.iter_mut().find(|i| i.id == removed_id) {
                stored.trash();
                Ok(true)
            } else {
                Ok(false)
            }
        })?;
        if !landed {
            anyhow::bail!(
                "Session {} was removed by another process before it could be trashed",
                removed_title
            );
        }
        println!(
            "  Moved session to trash: {} (from profile '{}')",
            removed_title,
            storage.profile()
        );
        println!(
            "  Restore with `aoe session restore {removed_id}`, or delete permanently with `aoe rm --purge {removed_id}`."
        );
        return Ok(());
    }

    let delete_worktree = needs_worktree_cleanup(&inst, &args);
    let delete_branch = inst
        .worktree_info
        .as_ref()
        .is_some_and(|wt| wt.managed_by_aoe)
        && (args.delete_branch || (delete_worktree && config.worktree.delete_branch_on_cleanup));
    let delete_sandbox = inst.sandbox_info.as_ref().is_some_and(|s| s.enabled)
        && !args.keep_container
        && config.sandbox.auto_cleanup;

    let result =
        crate::session::deletion::perform_deletion(&crate::session::deletion::DeletionRequest {
            session_id: inst.id.clone(),
            instance: inst.clone(),
            delete_worktree,
            delete_branch,
            delete_sandbox,
            force_delete: args.force,
            detach_hooks: false,
            keep_scratch: args.keep_scratch,
        });

    for msg in &result.messages {
        println!("  {}", msg);
    }
    for err in &result.errors {
        eprintln!("Warning: {}", err);
    }

    // Permanent purge of a structured-view session must also drop its durable
    // transcript so it does not orphan in the event store; the CLI opens the
    // store directly since it has no live worker. Only after a successful
    // teardown so a failed purge stays restorable. If the transcript can't be
    // dropped, keep the session row (skip the removal below) rather than
    // orphan the transcript. See #2489.
    if result.success {
        if let Err(e) = super::purge_acp_transcript(&inst) {
            anyhow::bail!(
                "Session teardown succeeded but its transcript could not be purged, so the session \
                 record was kept (retry, or remove it once the event store is reachable): {e}"
            );
        }
    }

    if !delete_worktree {
        if inst
            .worktree_info
            .as_ref()
            .is_some_and(|wt| wt.managed_by_aoe)
        {
            println!(
                "Worktree preserved at: {} (use --delete-worktree to remove)",
                inst.project_path
            );
        } else if let Some(ws_info) = &inst.workspace_info {
            if ws_info.cleanup_on_delete {
                println!(
                    "Workspace preserved at: {} (use --delete-worktree to remove)",
                    ws_info.workspace_dir
                );
            }
        }
    }
    if let Some(sandbox) = &inst.sandbox_info {
        if sandbox.enabled {
            if args.keep_container {
                println!("Container preserved: {}", sandbox.container_name);
            } else if !config.sandbox.auto_cleanup {
                println!(
                    "Container preserved: {} (auto_cleanup disabled in config)",
                    sandbox.container_name
                );
            }
        }
    }

    // Phase 2 (locked): drop the entry by id from the latest disk state.
    // No-op if a peer already removed it; that is the correct semantics.
    storage.update(|all_instances, _groups| {
        all_instances.retain(|i| i.id != removed_id);
        Ok(())
    })?;

    // Keep the project in the new-session wizard's Recent tab after its last
    // session is gone (#2141). Best-effort; a failure must not fail the remove.
    if let Some(entry) = crate::session::recent_project_entry_for(&inst) {
        if let Err(e) = crate::session::record_recent_project(entry) {
            tracing::warn!(target: "session.delete",
                "recording recent project after remove failed: {e}");
        }
    }

    println!(
        "  Removed session: {} (from profile '{}')",
        removed_title,
        storage.profile()
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{WorkspaceInfo, WorkspaceRepo};
    use chrono::Utc;

    fn args(delete_worktree: bool) -> RemoveArgs {
        RemoveArgs {
            identifier: "x".to_string(),
            delete_worktree,
            delete_branch: false,
            force: false,
            keep_container: false,
            keep_scratch: false,
            purge: false,
        }
    }

    // Regression for #2363: a multi-repo workspace session has no
    // `worktree_info`, so the old worktree_info-only check returned false and
    // `--delete-worktree` silently left the workspace dir on disk.
    #[test]
    fn needs_worktree_cleanup_true_for_workspace_session() {
        let mut inst = Instance::new("WS", "/tmp/ws/repo-a");
        inst.workspace_info = Some(WorkspaceInfo {
            branch: "feature/abc".to_string(),
            workspace_dir: "/tmp/ws".to_string(),
            repos: vec![WorkspaceRepo {
                name: "repo-a".to_string(),
                source_path: "/tmp/src/repo-a".to_string(),
                branch: "feature/abc".to_string(),
                worktree_path: "/tmp/ws/repo-a".to_string(),
                main_repo_path: "/tmp/src/repo-a".to_string(),
                managed_by_aoe: true,
            }],
            created_at: Utc::now(),
            cleanup_on_delete: true,
        });

        assert!(needs_worktree_cleanup(&inst, &args(true)));
        assert!(!needs_worktree_cleanup(&inst, &args(false)));
    }
}
