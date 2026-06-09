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
}

fn needs_worktree_cleanup(inst: &Instance, args: &RemoveArgs) -> bool {
    inst.worktree_info
        .as_ref()
        .is_some_and(|wt| wt.managed_by_aoe && args.delete_worktree)
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

    if !delete_worktree {
        if let Some(wt_info) = &inst.worktree_info {
            if wt_info.managed_by_aoe {
                println!(
                    "Worktree preserved at: {} (use --delete-worktree to remove)",
                    inst.project_path
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

    println!(
        "  Removed session: {} (from profile '{}')",
        removed_title,
        storage.profile()
    );

    Ok(())
}
