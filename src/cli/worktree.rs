//! `agent-of-empires worktree` command implementation

use anyhow::{bail, Result};
use clap::Subcommand;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::git::{GitWorktree, WorktreeEntry};
use crate::session::Storage;

#[derive(Subcommand)]
pub enum WorktreeCommands {
    /// List all worktrees in current repository
    #[command(alias = "ls")]
    List,

    /// Show worktree information for a session
    Info {
        /// Session ID or title
        identifier: String,
    },

    /// Cleanup orphaned worktrees
    Cleanup {
        /// Actually remove worktrees (default is dry-run)
        #[arg(short = 'f', long = "force")]
        force: bool,
    },
}

#[tracing::instrument(target = "cli.session", skip_all, fields(profile = %profile))]
pub async fn run(profile: &str, command: WorktreeCommands) -> Result<()> {
    match command {
        WorktreeCommands::List => list_worktrees().await,
        WorktreeCommands::Info { identifier } => show_info(profile, &identifier).await,
        WorktreeCommands::Cleanup { force } => cleanup_orphaned(profile, force).await,
    }
}

async fn list_worktrees() -> Result<()> {
    let current_dir = std::env::current_dir()?;

    if !GitWorktree::is_git_repo(&current_dir) {
        bail!("Not in a git repository\nTip: Navigate to a git repository first");
    }

    let main_repo = GitWorktree::find_main_repo(&current_dir)?;
    let git_wt = GitWorktree::new(main_repo)?;

    let worktrees = git_wt.list_worktrees()?;

    println!("Git Worktrees:\n");
    println!("{:<40} {:<30} {:<10}", "PATH", "BRANCH", "TYPE");
    println!("{}", "=".repeat(80));

    for wt in &worktrees {
        let branch = wt.branch.clone().unwrap_or_else(|| {
            if wt.is_detached {
                "(detached)".to_string()
            } else {
                "(unknown)".to_string()
            }
        });

        let wt_type = if wt.path == git_wt.repo_path {
            "main"
        } else {
            "worktree"
        };

        let shortened_path = crate::util::collapse_tilde(&wt.path.to_string_lossy());

        println!("{:<40} {:<30} {:<10}", shortened_path, branch, wt_type);
    }

    println!("\nTotal: {} worktrees", worktrees.len());

    Ok(())
}

async fn show_info(profile: &str, identifier: &str) -> Result<()> {
    let storage = Storage::open_unwatched(profile)?;
    let (instances, _) = storage.load_with_groups()?;

    let session = super::resolve_session(identifier, &instances)?;

    if let Some(wt_info) = &session.worktree_info {
        println!("Worktree Information:\n");
        println!("  Session:       {}", session.title);
        println!("  Branch:        {}", wt_info.branch);
        println!("  Worktree Path: {}", session.project_path);
        println!("  Main Repo:     {}", wt_info.main_repo_path);
        println!(
            "  Managed by aoe: {}",
            if wt_info.managed_by_aoe { "Yes" } else { "No" }
        );
        println!(
            "  Created at:    {}",
            wt_info.created_at.format("%Y-%m-%d %H:%M:%S")
        );

        let worktree_path = PathBuf::from(&session.project_path);
        if worktree_path.exists() {
            println!("\n  Status:        ✓ Worktree exists");
        } else {
            println!("\n  Status:        ✗ Worktree missing (orphaned session)");
            println!("  Tip:           Run 'aoe worktree cleanup' to remove orphaned sessions");
        }
    } else if let Some(ws_info) = &session.workspace_info {
        println!("Workspace Information:\n");
        println!("  Session:       {}", session.title);
        println!("  Branch:        {}", ws_info.branch);
        println!("  Workspace Dir: {}", ws_info.workspace_dir);
        println!("  Repos:         {}", ws_info.repos.len());
        println!(
            "  Cleanup on delete: {}",
            if ws_info.cleanup_on_delete {
                "Yes"
            } else {
                "No"
            }
        );
        println!(
            "  Created at:    {}",
            ws_info.created_at.format("%Y-%m-%d %H:%M:%S")
        );
        println!();
        for repo in &ws_info.repos {
            println!("  Repository: {}", repo.name);
            println!("    Source:    {}", repo.source_path);
            println!("    Worktree:  {}", repo.worktree_path);
            println!("    Main Repo: {}", repo.main_repo_path);
            println!(
                "    Managed:   {}",
                if repo.managed_by_aoe { "Yes" } else { "No" }
            );
            let wt_path = PathBuf::from(&repo.worktree_path);
            if wt_path.exists() {
                println!("    Status:    Exists");
            } else {
                println!("    Status:    Missing");
            }
            println!();
        }
    } else {
        bail!(
            "Session '{}' is not associated with a worktree",
            session.title
        );
    }

    Ok(())
}

fn partition_orphaned_worktrees(
    worktrees: Vec<WorktreeEntry>,
    main_repo: &Path,
    tracked_paths: &HashSet<String>,
    protected_branches: &HashSet<String>,
) -> (Vec<WorktreeEntry>, Vec<WorktreeEntry>) {
    worktrees
        .into_iter()
        .filter(|wt| {
            wt.path != main_repo && !tracked_paths.contains(&wt.path.to_string_lossy().to_string())
        })
        .partition(|wt| {
            !wt.branch
                .as_ref()
                .is_some_and(|b| protected_branches.contains(b))
        })
}

async fn cleanup_orphaned(profile: &str, force: bool) -> Result<()> {
    let storage = Storage::open_unwatched(profile)?;
    let (instances, _groups) = storage.load_with_groups()?;

    let mut orphaned_sessions = Vec::new();
    let mut orphaned_worktrees = Vec::new();

    for inst in &instances {
        if let Some(_wt_info) = &inst.worktree_info {
            let worktree_path = PathBuf::from(&inst.project_path);
            if !worktree_path.exists() {
                orphaned_sessions.push(inst.clone());
            }
        } else if let Some(ws_info) = &inst.workspace_info {
            let ws_path = PathBuf::from(&ws_info.workspace_dir);
            if !ws_path.exists() {
                orphaned_sessions.push(inst.clone());
            }
        }
    }

    let mut protected_worktrees = Vec::new();
    let current_dir = std::env::current_dir()?;
    if GitWorktree::is_git_repo(&current_dir) {
        let main_repo = GitWorktree::find_main_repo(&current_dir)?;
        let git_wt = GitWorktree::new(main_repo)?;
        let worktrees = git_wt.list_worktrees()?;
        let tracked: HashSet<String> = instances
            .iter()
            .map(|inst| inst.project_path.clone())
            .collect();

        (orphaned_worktrees, protected_worktrees) = partition_orphaned_worktrees(
            worktrees,
            &git_wt.repo_path,
            &tracked,
            &git_wt.protected_default_branch_names()?,
        );
    }

    if !protected_worktrees.is_empty() {
        println!("Skipped (default-branch checkouts, never removed):\n");
        for wt in &protected_worktrees {
            let unknown = "(unknown)".to_string();
            let branch = wt.branch.as_ref().unwrap_or(&unknown);
            println!("  • {}", wt.path.display());
            println!("    Branch: {}", branch);
        }
        println!();
    }

    if orphaned_sessions.is_empty() && orphaned_worktrees.is_empty() {
        println!("✓ No orphaned worktrees or sessions found");
        return Ok(());
    }

    if !orphaned_sessions.is_empty() {
        println!("Orphaned Sessions (worktree deleted but session remains):\n");
        for inst in &orphaned_sessions {
            println!("  • {} ({})", inst.title, inst.id);
            println!("    Missing worktree: {}", inst.project_path);
        }
        println!();
    }

    if !orphaned_worktrees.is_empty() {
        println!("Orphaned Worktrees (worktree exists but no session):\n");
        for wt in &orphaned_worktrees {
            let unknown = "(unknown)".to_string();
            let branch = wt.branch.as_ref().unwrap_or(&unknown);
            println!("  • {}", wt.path.display());
            println!("    Branch: {}", branch);
        }
        println!();
    }

    if !force {
        println!("This is a dry-run. Use --force to actually remove orphaned items.");
        println!();
        println!("What would be cleaned up:");
        println!("  - {} orphaned sessions", orphaned_sessions.len());
        println!("  - {} orphaned worktrees", orphaned_worktrees.len());
        return Ok(());
    }

    use std::io::{self, Write};

    print!("\nProceed with cleanup? This will:\n");
    println!("  - Remove {} sessions from aoe", orphaned_sessions.len());
    println!(
        "  - Delete {} worktree directories",
        orphaned_worktrees.len()
    );
    print!("(y/N): ");
    io::stdout().flush()?;

    let mut response = String::new();
    io::stdin().read_line(&mut response)?;
    let response = response.trim().to_lowercase();

    if response != "y" && response != "yes" {
        println!("Cleanup cancelled");
        return Ok(());
    }

    let ownership = crate::session::storage::acquire_ownership_lock()?;
    let _identity =
        crate::session::storage::acquire_session_identity_lock_with_ownership(&ownership)?;
    storage.verify_profile_identity()?;
    let mut removed_count = 0;

    if !orphaned_sessions.is_empty() {
        let removed = storage.update_with_ownership(&ownership, |all_instances, _groups| {
            let before = all_instances.len();
            all_instances.retain(|inst| {
                let Some(snapshot) = orphaned_sessions.iter().find(|old| old.id == inst.id) else {
                    return true;
                };
                if inst.created_at != snapshot.created_at
                    || inst.project_path != snapshot.project_path
                    || inst.workspace_info.as_ref().map(|workspace| {
                        (
                            &workspace.workspace_dir,
                            &workspace.branch,
                            &workspace.repos,
                            workspace.created_at,
                            workspace.cleanup_on_delete,
                        )
                    }) != snapshot.workspace_info.as_ref().map(|workspace| {
                        (
                            &workspace.workspace_dir,
                            &workspace.branch,
                            &workspace.repos,
                            workspace.created_at,
                            workspace.cleanup_on_delete,
                        )
                    })
                    || inst.worktree_info != snapshot.worktree_info
                    || inst.lifecycle_reservation != snapshot.lifecycle_reservation
                {
                    return true;
                }
                let path = inst
                    .workspace_info
                    .as_ref()
                    .map_or(Path::new(&inst.project_path), |workspace| {
                        Path::new(&workspace.workspace_dir)
                    });
                !matches!(path.try_exists(), Ok(false))
            });
            Ok(before - all_instances.len())
        })?;
        removed_count += removed;
        println!("✓ Removed {removed} orphaned sessions");
    }

    if !orphaned_worktrees.is_empty() {
        let current_dir = std::env::current_dir()?;
        let main_repo = GitWorktree::find_main_repo(&current_dir)?;
        let git_wt = GitWorktree::new(main_repo)?;
        removed_count += cleanup_worktrees_with_ownership(
            &git_wt,
            &orphaned_worktrees,
            storage.profile(),
            &ownership,
        )?;
    }

    println!("\n✓ Cleanup complete: {} items removed", removed_count);

    Ok(())
}

fn cleanup_worktrees_with_ownership(
    git_wt: &GitWorktree,
    candidates: &[WorktreeEntry],
    profile: &str,
    ownership: &crate::session::storage::OwnershipGuard,
) -> Result<usize> {
    let claims =
        crate::session::deletion::paths_in_use_except_with_ownership(ownership, profile, &[]);
    let live = git_wt.list_worktrees()?;
    let protected = git_wt.protected_default_branch_names()?;
    let mut removed = 0;
    for wt in candidates {
        if claims.covers(&wt.path)
            || !live
                .iter()
                .any(|current| current.path == wt.path && current.branch == wt.branch)
            || wt
                .branch
                .as_ref()
                .is_some_and(|branch| protected.contains(branch))
        {
            println!(
                "Retained claimed or changed worktree: {}",
                wt.path.display()
            );
            continue;
        }
        match git_wt.remove_worktree(&wt.path, true) {
            Ok(_) => {
                println!("✓ Removed worktree: {}", wt.path.display());
                removed += 1;
            }
            Err(e) => eprintln!("✗ Failed to remove {}: {}", wt.path.display(), e),
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[serial_test::serial]
    fn confirmed_cleanup_rechecks_late_and_unreadable_peer_claims() -> Result<()> {
        let _app = crate::session::test_support::isolate_app_dir();
        let temp = tempfile::tempdir()?;
        let repo_path = temp.path().join("repo");
        let repo = git2::Repository::init(&repo_path)?;
        let signature = git2::Signature::now("Test", "test@example.invalid")?;
        let tree_id = repo.index()?.write_tree()?;
        repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            "initial",
            &repo.find_tree(tree_id)?,
            &[],
        )?;
        let base = repo.head()?.shorthand().unwrap().to_owned();
        let git = GitWorktree::new(repo_path)?;
        let path = temp.path().join("candidate");
        git.create_worktree("candidate", &path, true, Some(&base))?;
        let candidates: Vec<_> = git
            .list_worktrees()?
            .into_iter()
            .filter(|worktree| worktree.branch.as_deref() == Some("candidate"))
            .collect();
        let cleaner = Storage::new_unwatched("cleaner")?;
        let peer = Storage::new_unwatched("late-peer")?;
        peer.update(|rows, _| {
            rows.push(crate::session::Instance::new(
                "peer",
                path.to_str().unwrap(),
            ));
            Ok(())
        })?;
        let ownership = crate::session::storage::acquire_ownership_lock()?;
        assert_eq!(
            cleanup_worktrees_with_ownership(&git, &candidates, cleaner.profile(), &ownership)?,
            0
        );
        assert!(path.is_dir());
        assert_eq!(peer.load()?[0].project_path, path.to_string_lossy());
        std::fs::write(
            peer.sessions_path(),
            r#"[{"project_path":"/unreadable-owner"}]"#,
        )?;
        assert_eq!(
            cleanup_worktrees_with_ownership(&git, &candidates, cleaner.profile(), &ownership)?,
            0
        );
        assert!(path.is_dir());
        std::fs::write(peer.sessions_path(), "[]")?;
        assert_eq!(
            cleanup_worktrees_with_ownership(&git, &candidates, cleaner.profile(), &ownership)?,
            1
        );
        assert!(!path.exists());
        assert!(git.branch_exists("candidate")?);
        Ok(())
    }

    fn entry(path: &str, branch: Option<&str>) -> WorktreeEntry {
        WorktreeEntry {
            path: PathBuf::from(path),
            branch: branch.map(str::to_string),
            is_detached: branch.is_none(),
        }
    }

    #[test]
    fn partition_orphaned_worktrees_keeps_the_default_branch_out_of_the_removal_list() {
        let worktrees = vec![
            entry("/p/.bare", Some("main")),
            entry("/p/main", Some("main")),
            entry("/p/wt/tracked", Some("feature/tracked")),
            entry("/p/wt/orphan", Some("feature/orphan")),
            entry("/p/wt/detached", None),
        ];
        let tracked = HashSet::from(["/p/wt/tracked".to_string()]);
        let protected = HashSet::from(["main".to_string()]);

        let (removable, kept) =
            partition_orphaned_worktrees(worktrees, Path::new("/p/.bare"), &tracked, &protected);

        assert_eq!(
            removable.iter().map(|w| w.path.clone()).collect::<Vec<_>>(),
            vec![
                PathBuf::from("/p/wt/orphan"),
                PathBuf::from("/p/wt/detached"),
            ],
            "the main worktree and tracked worktrees drop out; real orphans stay"
        );
        assert_eq!(
            kept.iter().map(|w| w.path.clone()).collect::<Vec<_>>(),
            vec![PathBuf::from("/p/main")]
        );
    }
}
