//! `agent-of-empires remove` command implementation

use anyhow::Result;
use chrono::Utc;
use clap::Args;

use crate::session::{Instance, LifecycleOperation, Storage};

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

/// Whether a `--purge` should delete the session's git branch(es). #2525: gated
/// on the shape-agnostic predicate so it fires for multi-repo workspace sessions
/// (only `workspace_info`, no `worktree_info`) as well as worktree sessions;
/// `perform_deletion` keys both worktree and workspace-repo branch cleanup off
/// this flag. The old `worktree_info`-only gate skipped workspace branches.
fn should_delete_branch(
    inst: &Instance,
    args: &RemoveArgs,
    delete_worktree: bool,
    delete_branch_on_cleanup: bool,
) -> bool {
    inst.has_managed_worktree_or_workspace()
        && (args.delete_branch || (delete_worktree && delete_branch_on_cleanup))
}

#[tracing::instrument(target = "cli.session", skip_all, fields(profile = %profile))]
pub async fn run(profile: &str, args: RemoveArgs) -> Result<()> {
    let storage = Storage::open_unwatched(profile)?;

    // Snapshot the target without holding either lock across user-facing
    // config resolution. The purge path below takes its durable claim, then
    // retains the per-instance lifecycle lock across every destructive stage.
    let (instances, _groups) = storage.load_with_groups()?;

    let mut inst = super::resolve_session(&args.identifier, &instances)
        .map_err(|e| anyhow::anyhow!("{} in profile '{}'", e, storage.profile()))?
        .clone();
    // Runtime-only source_profile is blank after deserialization; stamp the
    // explicitly selected profile before hooks or teardown resolve config.
    inst.source_profile = storage.profile().to_string();
    let removed_id = inst.id.clone();
    let removed_title = inst.title.clone();

    let config = crate::session::config::repo_config::resolve_config_with_repo_or_warn(
        profile,
        std::path::Path::new(&inst.project_path),
    );

    // Trash-first: unless --purge is given (or delete_to_trash is disabled),
    // stop the live session and mark it trashed, keeping every durable
    // artifact so it can be restored. Mirrors the archive CLI's tmux
    // teardown. See #2489.
    if config.session.delete_to_trash && !args.purge {
        let _lifecycle_lock = storage
            .acquire_instance_lifecycle_lock(&removed_id)
            .map_err(|error| anyhow::anyhow!("failed to acquire instance trash lock: {error}"))?;
        let trash_generation = storage.update(|all_instances, _groups| {
            let stored = all_instances
                .iter_mut()
                .find(|instance| instance.id == removed_id)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "Session {removed_title} was removed by another process before it could be trashed"
                    )
                })?;
            let generation = stored
                .try_acquire_lifecycle_reservation(
                    LifecycleOperation::Trash,
                    Instance::LIFECYCLE_RESERVATION_TTL,
                    Utc::now(),
                )
                .map_err(|error| anyhow::anyhow!("Session {removed_title}: {error}"))?;
            stored.trash();
            Ok(generation)
        })?;
        if let Err(error) = inst.kill_locked() {
            eprintln!("Warning: failed to kill agent tmux session: {error}");
        }
        inst.kill_ancillary_tmux_sessions_locked();

        // The session is durably trashed; stop its sandbox container (so it
        // doesn't keep running for the whole retention window) and move its
        // worktree out of the active dir into the holding area, then persist the
        // repointed project_path. Stopping the container also releases the
        // worktree bind mount, without which the git move hits EBUSY. A failure
        // here never blocks the trash; the worktree just stays in place and a
        // later reconcile pass can relocate it.
        let mut inst = inst;
        inst.trash();
        // The teardown's still-trashed re-check reads storage via
        // `source_profile`; stamp it so a `-p <profile>` remove re-checks the
        // profile it actually trashed the row in, not the default.
        inst.source_profile = storage.profile().to_string();
        match crate::session::trash::prepare_trashed_worktree(&mut inst) {
            crate::session::trash::RelocateOutcome::Relocated { .. } => {
                // Atomic durable check-and-commit: a peer restore or purge can
                // land between the teardown's pre-move re-check and this
                // persist, so the decision is re-taken on the durable row
                // under the flock. Superseded means such a peer won and the
                // move is undone rather than recorded onto a live row.
                let reloc = crate::session::trash::TrashRelocation {
                    new_project_path: inst.project_path.clone(),
                    pre_trash_project_path: inst.pre_trash_project_path.clone(),
                };
                // The decision travels through this captured slot rather than
                // the closure's return value, so it survives an update that
                // decided Superseded and then failed its final write; the
                // undo keys off the decision alone, since the durable row was
                // already restored in that case. A Persisted decision whose
                // write failed needs no repair: the row is still trashed at
                // its old path and the next reconcile repoints it.
                let mut decided: Option<crate::session::claim::RelocationCommit> = None;
                let update_result = storage.update(|all_instances, _groups| {
                    decided = Some(crate::session::claim::commit_trash_relocation(
                        all_instances,
                        &removed_id,
                        trash_generation,
                        &reloc,
                    ));
                    Ok(())
                });
                if let Err(e) = &update_result {
                    eprintln!(
                        "  Note: could not persist the trash relocation ({e}); it will be reconciled on next load."
                    );
                }
                if matches!(
                    decided,
                    Some(crate::session::claim::RelocationCommit::Superseded)
                ) {
                    match crate::session::trash::undo_raced_relocation(&inst, &reloc) {
                        crate::session::trash::RestoreOutcome::Failed { reason } => {
                            eprintln!(
                                "  Note: a concurrent restore superseded the trash; could not move the worktree back ({reason})."
                            );
                        }
                        _ => {
                            eprintln!(
                                "  Note: a concurrent restore superseded the trash; the worktree was left in place."
                            );
                        }
                    }
                    // The trash was superseded: the session is live again, so
                    // the "moved to trash" summary and restore hint below
                    // would be contradictory.
                    println!(
                        "  Session was restored by another process; it was not moved to the trash."
                    );
                    return Ok(());
                }
            }
            crate::session::trash::RelocateOutcome::Failed { reason } => {
                eprintln!("  Note: left worktree in place ({reason}).");
                release_trash_reservation_best_effort(&storage, &removed_id, trash_generation);
            }
            crate::session::trash::RelocateOutcome::Skipped => {
                release_trash_reservation_best_effort(&storage, &removed_id, trash_generation);
            }
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
    let delete_branch = should_delete_branch(
        &inst,
        &args,
        delete_worktree,
        config.worktree.delete_branch_on_cleanup,
    );
    let delete_sandbox = inst.sandbox_info.as_ref().is_some_and(|s| s.enabled)
        && !args.keep_container
        && config.sandbox.auto_cleanup;

    let storage_profile = storage.profile().to_string();
    let reservation = crate::session::deletion::PurgeTransaction::reserve(
        storage,
        crate::session::deletion::DeletionRequest {
            session_id: inst.id.clone(),
            instance: inst.clone(),
            delete_worktree,
            delete_branch,
            delete_sandbox,
            force_delete: args.force,
            detach_hooks: false,
            keep_scratch: args.keep_scratch,
        },
    )?;
    let transaction = match reservation {
        crate::session::deletion::PurgeReservation::Reserved(transaction) => transaction,
        crate::session::deletion::PurgeReservation::Rejected(result) => {
            let detail = result
                .errors
                .first()
                .map(String::as_str)
                .unwrap_or("Session purge was refused");
            anyhow::bail!("{detail}: {removed_title}");
        }
    };
    let result = transaction.run_hooks().complete_with(|instance| {
        super::purge_acp_transcript(instance).map_err(|error| {
            format!(
                "Session teardown succeeded but its transcript could not be purged, so the session \
                 record was kept (retry, or remove it once the event store is reachable): {error}"
            )
        })
    });

    for msg in &result.messages {
        println!("  {}", msg);
    }
    for err in &result.errors {
        eprintln!("Warning: {}", err);
    }

    if result.disposition == crate::session::deletion::DeletionDisposition::KeptRestored {
        eprintln!(
            "Warning: session {} was restored while its purge was running; kept the \
             restored record, but its worktree, branch, container, or transcript may \
             already have been removed by the purge. Inspect and repair it.",
            removed_title
        );
        return Ok(());
    }
    if result.disposition == crate::session::deletion::DeletionDisposition::AlreadyGone {
        return Ok(());
    }
    if !result.success {
        anyhow::bail!(
            "Session teardown failed, so the session record was kept (retry, or fix the \
             underlying cause and remove it again)"
        );
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

    // The transaction durably removed the row before returning. Keep the
    // project in the new-session wizard's Recent tab after its last session is
    // gone (#2141). Best-effort; a failure must not fail the remove.
    if let Some(entry) = crate::session::recent_project_entry_for(&inst) {
        if let Err(e) = crate::session::record_recent_project(entry) {
            tracing::warn!(target: "session.delete",
                "recording recent project after remove failed: {e}");
        }
    }

    println!(
        "  Removed session: {} (from profile '{}')",
        removed_title, storage_profile
    );

    Ok(())
}

/// Release the teardown's trash reservation on a no-relocation terminal path.
fn release_trash_reservation_best_effort(storage: &Storage, removed_id: &str, generation: u64) {
    let _ = storage.update(|all_instances, _groups| {
        crate::session::claim::release_trash_reservation(all_instances, removed_id, generation);
        Ok(())
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{WorkspaceInfo, WorkspaceRepo};

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
                branch_preexisting: false,
                base_branch: None,
                base_branch_override: None,
            }],
            created_at: Utc::now(),
            cleanup_on_delete: true,
        });

        assert!(needs_worktree_cleanup(&inst, &args(true)));
        assert!(!needs_worktree_cleanup(&inst, &args(false)));
    }

    // Regression for #2525: a multi-repo workspace session has no
    // `worktree_info`, so the old `worktree_info`-only gate returned false and
    // `--purge --delete-worktree --delete-branch` left the AoE-created branches
    // behind. The shape-agnostic gate must enable branch deletion for it.
    #[test]
    fn should_delete_branch_true_for_workspace_session() {
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
                branch_preexisting: false,
                base_branch: None,
                base_branch_override: None,
            }],
            created_at: Utc::now(),
            cleanup_on_delete: true,
        });

        let mut with_flag = args(true);
        with_flag.delete_branch = true;
        // Explicit --delete-branch fires regardless of the config default.
        assert!(should_delete_branch(&inst, &with_flag, true, false));
        // And via the config default when deleting the worktree.
        assert!(should_delete_branch(&inst, &args(true), true, true));
        // Not without any managed worktree/workspace.
        let plain = Instance::new("plain", "/tmp/plain");
        assert!(!should_delete_branch(&plain, &with_flag, true, true));
    }

    // A direct `rm --purge` of a genuinely live session (was_trashed=false)
    // has no restore to lose to, so it proceeds and claims. The bailing,
    // fresh-restore-refusal, and finalize cases live with the fns in
    // `session::claim`. See #2541.
    #[test]
    fn rm_purge_of_live_session_still_proceeds() {
        let live = Instance::new("s", "/tmp/x");
        let id = live.id.clone();
        let mut all = vec![live];
        assert!(matches!(
            crate::session::claim::decide_purge_claim(&mut all, &id, false, Utc::now()).unwrap(),
            crate::session::claim::PurgeClaimDecision::Claimed(1)
        ));
        assert_eq!(
            all[0].lifecycle_reservation.as_ref().map(|c| c.op),
            Some(LifecycleOperation::Purge)
        );
    }
}
