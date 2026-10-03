//! `agent-of-empires remove` command implementation

use anyhow::Result;
use chrono::Utc;
use clap::Args;

use crate::session::{acquire_session_identity_lock, Instance, LifecycleOperation, Storage};

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

    /// Permanently delete instead of moving to trash.
    ///
    /// By default `rm` moves the session to the trash (when
    /// `session.delete_to_trash` is enabled, the default) so it can be
    /// restored; `--purge` forces the irreversible
    /// teardown (worktree/branch/container cleanup per the other flags) and
    /// removes the session's structured-view transcript. Removing the sandbox
    /// container also attempts to remove its private agent stores, including
    /// its config home; keeping the container keeps those stores. Host agent
    /// conversation history outside these stores is not removed.
    #[arg(long)]
    purge: bool,
}

fn needs_worktree_cleanup(inst: &Instance, args: &RemoveArgs) -> bool {
    args.delete_worktree && inst.has_managed_worktree_or_workspace()
}

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

    let (instances, _groups) = storage.load_with_groups()?;

    let mut inst = super::resolve_session(&args.identifier, &instances)
        .map_err(|e| anyhow::anyhow!("{} in profile '{}'", e, storage.profile()))?
        .clone();
    inst.source_profile = storage.profile().to_string();
    let removed_id = inst.id.clone();
    let removed_title = inst.title.clone();

    let config = crate::session::config::repo_config::resolve_config_with_repo_or_warn(
        profile,
        std::path::Path::new(&inst.project_path),
    );

    if config.session.delete_to_trash && !args.purge {
        let (trash_generation, plan) = {
            let _workspace_claim_lock = crate::session::acquire_session_workspace_claim_lock()?;
            let _identity_lock = acquire_session_identity_lock()?;
            let _lifecycle_lock = storage
                .acquire_instance_lifecycle_lock(&removed_id)
                .map_err(|error| {
                    anyhow::anyhow!("failed to acquire instance trash lock: {error}")
                })?;
            let snapshot = storage
                .load_strict_for_worktree_ownership_locked()?
                .into_iter()
                .find(|row| row.id == removed_id)
                .ok_or_else(|| {
                    anyhow::anyhow!("Session {removed_title} disappeared before trash")
                })?;
            ensure_trash_paths_unclaimed(&storage, &snapshot)?;
            storage.update(|all_instances, _groups| {
                let stored = all_instances
                    .iter_mut()
                    .find(|instance| instance.id == removed_id)
                    .ok_or_else(|| {
                        anyhow::anyhow!("Session {removed_title} disappeared before trash")
                    })?;
                anyhow::ensure!(
                    trash_plan_unchanged(&snapshot, stored),
                    "Session {removed_title} changed before trash"
                );
                let generation = stored
                    .try_acquire_lifecycle_reservation(
                        LifecycleOperation::Trash,
                        Instance::LIFECYCLE_RESERVATION_TTL,
                        Utc::now(),
                    )
                    .map_err(|error| anyhow::anyhow!("Session {removed_title}: {error}"))?;
                stored.trash();
                Ok((generation, stored.clone()))
            })?
        };
        // The durable claim excludes launches while every flock is released for the stop wait.
        let settled = crate::session::runner_journal::settle(
            crate::session::deletion::SessionPathOwner {
                profile: storage.profile(),
                session_id: &removed_id,
            },
            Some((LifecycleOperation::Trash, trash_generation)),
        )
        .await;
        let _workspace_claim_lock = crate::session::acquire_session_workspace_claim_lock()?;
        let _identity_lock = acquire_session_identity_lock()?;
        let _lifecycle_lock = storage.acquire_instance_lifecycle_lock(&removed_id)?;
        let snapshot = storage
            .load_strict_for_worktree_ownership_locked()?
            .into_iter()
            .find(|row| row.id == removed_id)
            .ok_or_else(|| anyhow::anyhow!("Session {removed_title} disappeared during trash"))?;
        anyhow::ensure!(
            snapshot.is_trashed()
                && snapshot
                    .lifecycle_reservation_is_owned(LifecycleOperation::Trash, trash_generation)
                && trash_plan_unchanged(&plan, &snapshot),
            "Session {removed_title} lost its trash reservation or relocation plan"
        );
        if let Err(error) = settled {
            release_trash_reservation_best_effort(&storage, &removed_id, trash_generation);
            eprintln!(
                "  Note: left worktree in place because runner shutdown is unproven ({error})."
            );
        } else {
            if let Err(error) = ensure_trash_paths_unclaimed(&storage, &snapshot) {
                storage.update(|all_instances, _groups| {
                    if let Some(stored) = all_instances.iter_mut().find(|row| row.id == removed_id)
                    {
                        if stored.lifecycle_reservation_is_owned(
                            LifecycleOperation::Trash,
                            trash_generation,
                        ) {
                            stored.untrash();
                            stored.release_lifecycle_reservation_if_owned(
                                LifecycleOperation::Trash,
                                trash_generation,
                            );
                        }
                    }
                    Ok(())
                })?;
                return Err(error);
            }
            let outcome = storage.update(|all_instances, _groups| {
                let stored = all_instances
                    .iter()
                    .find(|row| row.id == removed_id)
                    .ok_or_else(|| {
                        anyhow::anyhow!("Session {removed_title} disappeared before relocation")
                    })?;
                anyhow::ensure!(
                    stored.is_trashed()
                        && stored.lifecycle_reservation_is_owned(
                            LifecycleOperation::Trash,
                            trash_generation
                        )
                        && trash_plan_unchanged(&snapshot, stored),
                    "Session {removed_title} lost its trash reservation or relocation plan"
                );
                if !stored.runner_journal.proves_quiescent() {
                    crate::session::claim::release_trash_reservation(
                        all_instances,
                        &removed_id,
                        trash_generation,
                    );
                    return Ok(crate::session::trash::RelocateOutcome::Failed {
                        reason: "durable runner history does not prove checkout quiescence"
                            .to_string(),
                    });
                }
                let mut instance = stored.clone();
                instance.source_profile = storage.profile().to_string();
                instance.kill_all_tmux_sessions_locked();
                let outcome = crate::session::trash::prepare_trashed_worktree(&mut instance);
                if matches!(
                    outcome,
                    crate::session::trash::RelocateOutcome::Relocated { .. }
                ) {
                    let relocation = crate::session::trash::TrashRelocation {
                        new_project_path: instance.project_path.clone(),
                        pre_trash_project_path: instance.pre_trash_project_path.clone(),
                    };
                    anyhow::ensure!(
                        crate::session::claim::commit_trash_relocation(
                            all_instances,
                            &removed_id,
                            trash_generation,
                            &relocation,
                        ) == crate::session::claim::RelocationCommit::Persisted,
                        "Session {removed_title} lost its trash reservation during relocation"
                    );
                } else {
                    crate::session::claim::release_trash_reservation(
                        all_instances,
                        &removed_id,
                        trash_generation,
                    );
                }
                Ok(outcome)
            })?;
            if let crate::session::trash::RelocateOutcome::Failed { reason } = outcome {
                eprintln!("  Note: left worktree in place ({reason}).");
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
    // Same barrier as the daemon and the TUI: a structured session's runner has
    // to be proven dead before the hooks play, since `on_destroy` scripts are
    // the user's and are not idempotent.
    let transaction = match crate::session::deletion::settle_runner_of(transaction).await {
        Ok(transaction) => transaction,
        Err(result) => {
            let detail = result
                .errors
                .first()
                .map(String::as_str)
                .unwrap_or("The session's agent could not be proven dead");
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

fn trash_plan_unchanged(snapshot: &Instance, durable: &Instance) -> bool {
    snapshot.is_trashed() == durable.is_trashed()
        && snapshot.project_path == durable.project_path
        && snapshot.pre_trash_project_path == durable.pre_trash_project_path
        && snapshot.worktree_info == durable.worktree_info
        && snapshot.scratch == durable.scratch
        && snapshot
            .workspace_info
            .as_ref()
            .map(|workspace| (&workspace.workspace_dir, &workspace.repos))
            == durable
                .workspace_info
                .as_ref()
                .map(|workspace| (&workspace.workspace_dir, &workspace.repos))
        && snapshot.is_sandboxed() == durable.is_sandboxed()
}

fn ensure_trash_paths_unclaimed(storage: &Storage, instance: &Instance) -> Result<()> {
    if !instance.has_managed_worktree_or_workspace() {
        return Ok(());
    }
    let mut paths = vec![std::path::PathBuf::from(&instance.project_path)];
    if let Some(workspace) = &instance.workspace_info {
        paths.push(std::path::PathBuf::from(&workspace.workspace_dir));
    }
    paths.extend(
        instance
            .all_repos()
            .iter()
            .map(|repo| std::path::PathBuf::from(&repo.worktree_path)),
    );
    if let Some(original) = &instance.pre_trash_project_path {
        paths.push(std::path::PathBuf::from(original));
    } else if let Some(holding) = crate::session::trash::trash_holding_path(
        std::path::Path::new(&instance.project_path),
        &instance.id,
    ) {
        paths.push(holding);
    }
    crate::session::deletion::ensure_unclaimed_paths(
        crate::session::deletion::SessionPathOwner { profile: storage.profile(), session_id: &instance.id },
        &paths,
    ).map_err(|error| anyhow::anyhow!(
        "Session {} was not moved to trash because worktree ownership could not be verified: {error}",
        instance.title,
    ))
}

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

    fn workspace_session() -> Instance {
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
        inst
    }

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

    #[test]
    fn needs_worktree_cleanup_true_for_workspace_session() {
        let inst = workspace_session();

        assert!(needs_worktree_cleanup(&inst, &args(true)));
        assert!(!needs_worktree_cleanup(&inst, &args(false)));
    }

    #[test]
    fn should_delete_branch_true_for_workspace_session() {
        let inst = workspace_session();

        let mut with_flag = args(true);
        with_flag.delete_branch = true;
        assert!(should_delete_branch(&inst, &with_flag, true, false));
        assert!(should_delete_branch(&inst, &args(true), true, true));
        let plain = Instance::new("plain", "/tmp/plain");
        assert!(!should_delete_branch(&plain, &with_flag, true, true));
    }

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
