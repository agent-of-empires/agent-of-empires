//! `agent-of-empires worktree` command implementation

use anyhow::{bail, Result};
use clap::Subcommand;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::git::GitWorktree;
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

        let shortened_path = shorten_path(&wt.path);

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

        // Check if worktree still exists
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

async fn cleanup_orphaned(profile: &str, force: bool) -> Result<()> {
    let storage = Storage::open_unwatched(profile)?;
    let (instances, _groups) = storage.load_with_groups()?;

    let mut orphaned_sessions = Vec::new();
    let mut orphaned_worktrees = Vec::new();

    // Find sessions with missing worktrees
    for inst in &instances {
        if let Some(_wt_info) = &inst.worktree_info {
            let worktree_path = PathBuf::from(&inst.project_path);
            if !worktree_path.exists() {
                orphaned_sessions.push(inst.clone());
            }
        } else if let Some(ws_info) = &inst.workspace_info {
            // Check if workspace dir exists
            let ws_path = PathBuf::from(&ws_info.workspace_dir);
            if !ws_path.exists() {
                orphaned_sessions.push(inst.clone());
            }
        }
    }

    // Find worktrees not associated with any session
    let current_dir = std::env::current_dir()?;
    if GitWorktree::is_git_repo(&current_dir) {
        let main_repo = GitWorktree::find_main_repo(&current_dir)?;
        let git_wt = GitWorktree::new(main_repo)?;
        let worktrees = git_wt.list_worktrees()?;

        for wt in worktrees {
            let is_main = wt.path == git_wt.repo_path;
            if is_main {
                continue;
            }

            let wt_path_str = wt.path.to_string_lossy().to_string();
            let is_tracked = instances
                .iter()
                .any(|inst| inst.project_path == wt_path_str);

            if !is_tracked {
                orphaned_worktrees.push(wt);
            }
        }
    }

    if orphaned_sessions.is_empty() && orphaned_worktrees.is_empty() {
        println!("✓ No orphaned worktrees or sessions found");
        return Ok(());
    }

    // Report findings
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

    // Actual cleanup with force flag
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

    let mut removed_count = 0;

    // Remove orphaned sessions
    if !orphaned_sessions.is_empty() {
        let orphan_ids: HashSet<String> = orphaned_sessions.iter().map(|o| o.id.clone()).collect();
        storage.update(|all_instances, _groups| {
            all_instances.retain(|inst| !orphan_ids.contains(&inst.id));
            Ok(())
        })?;

        removed_count += orphaned_sessions.len();
        println!("✓ Removed {} orphaned sessions", orphaned_sessions.len());
    }

    // Remove orphaned worktrees
    if !orphaned_worktrees.is_empty() {
        let current_dir = std::env::current_dir()?;
        let main_repo = GitWorktree::find_main_repo(&current_dir)?;
        let git_wt = GitWorktree::new(main_repo)?;

        for wt in &orphaned_worktrees {
            match git_wt.remove_worktree(&wt.path, true) {
                Ok(_) => {
                    println!("✓ Removed worktree: {}", wt.path.display());
                    removed_count += 1;
                }
                Err(e) => {
                    eprintln!("✗ Failed to remove {}: {}", wt.path.display(), e);
                }
            }
        }
    }

    println!("\n✓ Cleanup complete: {} items removed", removed_count);

    Ok(())
}

fn shorten_path(path: &Path) -> String {
    let path_str = path.to_string_lossy();
    if let Some(home) = dirs::home_dir() {
        if let Some(home_str) = home.to_str() {
            if let Some(stripped) = path_str.strip_prefix(home_str) {
                return format!("~{}", stripped);
            }
        }
    }
    path_str.to_string()
}
