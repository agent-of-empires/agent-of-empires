//! `agent-of-empires worktree` command implementation

use anyhow::{bail, Result};
use clap::Subcommand;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::git::{GitWorktree, WorktreeEntry};
use crate::session::{Instance, Storage};

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
    let storage = Storage::open_reference(profile)?;
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

/// Where a purge moves `inst`'s worktree. A row whose checkout is there is mid-purge or awaiting
/// its retry, so neither is orphaned.
fn purge_aside(inst: &Instance) -> Option<PathBuf> {
    let path = Path::new(&inst.project_path);
    path.parent()?
        .join(format!(".aoe-purge-{}", inst.id))
        .canonicalize()
        .ok()
}

/// A row is orphaned once its checkout is gone and no lifecycle operation, such as a purge that
/// may restore it, has it reserved.
fn is_orphaned_session(inst: &Instance, now: chrono::DateTime<chrono::Utc>) -> bool {
    if inst.has_fresh_lifecycle_reservation(now) {
        return false;
    }
    if inst.worktree_info.is_some() {
        !Path::new(&inst.project_path).exists() && purge_aside(inst).is_none()
    } else if let Some(ws_info) = &inst.workspace_info {
        !Path::new(&ws_info.workspace_dir).exists()
    } else {
        false
    }
}

pub(crate) fn find_orphaned_sessions(instances: &[Instance]) -> Vec<Instance> {
    let now = chrono::Utc::now();
    instances
        .iter()
        .filter(|inst| is_orphaned_session(inst, now))
        .cloned()
        .collect()
}

/// Removes the listed rows that are still orphaned, judged again under each row's lifecycle lock
/// and the store lock. Returns how many were removed.
pub(crate) fn remove_orphaned_sessions(storage: &Storage, orphans: &[Instance]) -> Result<usize> {
    let mut held = Vec::new();
    let mut ids = HashSet::new();
    for orphan in orphans {
        match storage.try_acquire_instance_lifecycle_lock(&orphan.id)? {
            Some(lock) => {
                held.push(lock);
                ids.insert(orphan.id.clone());
            }
            None => eprintln!("✗ Skipped session {}: it is busy", orphan.id),
        }
    }
    let mut removed = 0;
    storage.update(|instances, _groups| {
        let now = chrono::Utc::now();
        let before = instances.len();
        instances.retain(|inst| !(ids.contains(&inst.id) && is_orphaned_session(inst, now)));
        removed = before - instances.len();
        Ok(())
    })?;
    Ok(removed)
}

/// The removable and the protected orphaned worktrees of `git_wt`'s repository.
pub(crate) fn find_orphaned_worktrees(
    instances: &[Instance],
    git_wt: &GitWorktree,
) -> Result<(Vec<WorktreeEntry>, Vec<WorktreeEntry>)> {
    let tracked: HashSet<String> = instances
        .iter()
        .flat_map(|inst| {
            let aside = purge_aside(inst).map(|aside| aside.to_string_lossy().into_owned());
            std::iter::once(inst.project_path.clone()).chain(aside)
        })
        .collect();
    Ok(partition_orphaned_worktrees(
        git_wt.list_worktrees_with_abandoned_purges()?,
        &git_wt.repo_path,
        &tracked,
        &git_wt.protected_default_branch_names()?,
    ))
}

async fn cleanup_orphaned(profile: &str, force: bool) -> Result<()> {
    let storage = Storage::open_unwatched(profile)?;
    let (instances, _groups) = storage.load_with_groups()?;

    let orphaned_sessions = find_orphaned_sessions(&instances);
    let mut orphaned_worktrees = Vec::new();
    let mut protected_worktrees = Vec::new();
    let current_dir = std::env::current_dir()?;
    if GitWorktree::is_git_repo(&current_dir) {
        let main_repo = GitWorktree::find_main_repo(&current_dir)?;
        let git_wt = GitWorktree::new(main_repo)?;
        (orphaned_worktrees, protected_worktrees) = find_orphaned_worktrees(&instances, &git_wt)?;
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

    let mut removed_count = 0;

    if !orphaned_sessions.is_empty() {
        let removed = remove_orphaned_sessions(&storage, &orphaned_sessions)?;
        removed_count += removed;
        println!("✓ Removed {removed} orphaned sessions");
    }

    if !orphaned_worktrees.is_empty() {
        let current_dir = std::env::current_dir()?;
        let main_repo = GitWorktree::find_main_repo(&current_dir)?;
        let git_wt = GitWorktree::new(main_repo)?;

        for wt in &orphaned_worktrees {
            // A purge resumed since the listing still owns its checkout.
            let claim = GitWorktree::claim_abandoned_purge(&wt.path);
            if claim.is_none() && GitWorktree::is_purging(&wt.path) {
                eprintln!("✗ Skipped {}: its purge is in progress", wt.path.display());
                continue;
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, branch: Option<&str>) -> WorktreeEntry {
        WorktreeEntry {
            path: PathBuf::from(path),
            branch: branch.map(str::to_string),
            is_detached: branch.is_none(),
        }
    }

    /// Cleanup lists rows, then removes them only if they are still orphaned: a purge may restore a
    /// checkout in between, or hold the row while it runs.
    #[test]
    #[serial_test::serial]
    fn cleanup_removes_only_rows_still_orphaned() {
        let tmp = tempfile::TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(&tmp.path().join("home"));
        let storage = Storage::new_unwatched("cleanup").unwrap();
        let row = |name: &str| {
            let mut inst = Instance::new(name, tmp.path().join(name).to_str().unwrap());
            inst.worktree_info = Some(crate::session::WorktreeInfo {
                branch: name.to_string(),
                main_repo_path: tmp.path().join("main").to_string_lossy().into_owned(),
                managed_by_aoe: true,
                created_at: chrono::Utc::now(),
                base_branch: None,
            });
            inst
        };
        let (gone, restored, locked, mut reserved) =
            (row("gone"), row("restored"), row("locked"), row("reserved"));
        reserved
            .try_acquire_lifecycle_reservation(
                crate::session::LifecycleOperation::Purge,
                Instance::LIFECYCLE_RESERVATION_TTL,
                chrono::Utc::now(),
            )
            .unwrap();
        let rows = vec![gone.clone(), restored.clone(), locked.clone(), reserved];
        storage
            .update(|instances, _groups| {
                instances.extend(rows.clone());
                Ok(())
            })
            .unwrap();

        let orphans = find_orphaned_sessions(&rows);
        let ids = |rows: &[Instance]| rows.iter().map(|r| r.title.clone()).collect::<Vec<_>>();
        assert_eq!(ids(&orphans), ["gone", "restored", "locked"]);

        std::fs::create_dir(&restored.project_path).unwrap();
        let _purging = storage.acquire_instance_lifecycle_lock(&locked.id).unwrap();
        assert_eq!(remove_orphaned_sessions(&storage, &orphans).unwrap(), 1);
        let left = storage.load().unwrap();
        assert_eq!(ids(&left), ["restored", "locked", "reserved"]);
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
