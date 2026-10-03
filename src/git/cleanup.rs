//! Shared worktree cleanup utilities used by both CLI and TUI deletion paths.

use std::path::{Path, PathBuf};

use crate::containers::DockerContainer;
use crate::session::path_identity::{canonicalize_or_raw, CleanupProtection};
use crate::session::Instance;

use super::open_repo_at;
use super::GitWorktree;

#[derive(Clone, Copy)]
pub(crate) struct WorktreeCleanupOptions {
    pub force: bool,
    pub allow_container_removal: bool,
    pub allow_root_cleanup: bool,
}

/// Cap on the number of dirty file entries we list inline in error messages so
/// the TUI output pane does not get blown out on a worktree with thousands of
/// changes (e.g., `target/` accidentally tracked).
const MAX_DIRTY_FILES_LISTED: usize = 30;

/// Cap on how many empty parent directories `prune_empty_parent_dirs` will
/// climb after a worktree removal. Shallow templates need 0-1 hops; deeper
/// nested templates like `../{repo-name}-worktrees/{branch}/{repo-name}` need
/// 2. Higher than that suggests a pathological template and we'd rather stop
/// than walk too far up the user's filesystem.
const MAX_PARENT_PRUNE_HOPS: usize = 4;

/// Walk up from a removed worktree path, deleting empty wrapper directories
/// that `git worktree add` created as a side effect of a nested path template.
///
/// Empty-only by design: uses `remove_dir`, never `remove_dir_all`. Anything
/// non-empty (e.g., a sibling repo cloned by an `on_create` hook) keeps the
/// wrapper alive so the user can decide what to do with the orphan.
///
/// Stops on:
/// - First non-empty / inaccessible parent
/// - Any directory that is `main_repo` itself or an ancestor of it
/// - The user's home directory or any of its ancestors
/// - Filesystem root
/// - `MAX_PARENT_PRUNE_HOPS` levels climbed
///
/// Best-effort: failures are logged and swallowed. The caller's worktree
/// removal already succeeded; an orphaned wrapper is a cosmetic leak, not a
/// reason to fail the deletion.
fn prune_empty_parent_dirs(
    worktree_path: &Path,
    main_repo: &Path,
    protection: &[&CleanupProtection],
) {
    let main_canonical = canonicalize_or_raw(main_repo);
    let home = dirs::home_dir();

    let mut current = worktree_path.parent().map(|p| p.to_path_buf());
    let mut hops = 0;

    while let Some(parent) = current {
        if hops >= MAX_PARENT_PRUNE_HOPS {
            break;
        }

        // Filesystem root has no parent; never try to remove it.
        if parent.parent().is_none() {
            break;
        }

        let parent_canonical = canonicalize_or_raw(&parent);
        if protection
            .iter()
            .any(|owner| owner.references_path(&parent))
        {
            break;
        }

        // Refuse to touch the main repo or any of its ancestors.
        if main_canonical.starts_with(&parent_canonical) {
            break;
        }

        // Refuse to touch the user's home dir or any of its ancestors.
        if let Some(h) = &home {
            if h.starts_with(&parent_canonical) {
                break;
            }
        }

        match std::fs::remove_dir(&parent) {
            Ok(()) => {
                tracing::debug!(target: "git.worktree",
                    path = %parent.display(),
                    "removed empty worktree wrapper dir"
                );
                current = parent.parent().map(|p| p.to_path_buf());
                hops += 1;
            }
            Err(e) => {
                tracing::debug!(target: "git.worktree",
                    path = %parent.display(),
                    error = %e,
                    "stopped pruning at non-empty or inaccessible parent"
                );
                break;
            }
        }
    }
}

/// Remove a worktree directory from the filesystem.
///
/// Always tries `remove_dir` first (fast path for empty dirs). When `force`
/// is true, falls back to `remove_dir_all` for non-empty directories.
/// Refuses to delete the directory if it is the main repo itself.
///
/// On failure, retries a few times with short delays to handle macOS
/// Docker Desktop VirtioFS propagation delays after container removal.
pub fn remove_worktree_dir(
    worktree_path: &Path,
    main_repo: &Path,
    force: bool,
) -> std::io::Result<()> {
    let wt = worktree_path
        .canonicalize()
        .unwrap_or(worktree_path.to_path_buf());
    let mr = main_repo.canonicalize().unwrap_or(main_repo.to_path_buf());
    if wt == mr {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "worktree path is the same as the main repo -- refusing to delete",
        ));
    }

    for attempt in 0..5 {
        if !worktree_path.exists() {
            return Ok(());
        }
        let result = std::fs::remove_dir(worktree_path);
        if result.is_ok() {
            return Ok(());
        }
        if force {
            let result = std::fs::remove_dir_all(worktree_path);
            if result.is_ok() {
                return Ok(());
            }
        }
        if attempt < 4 {
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
    }

    // Final attempt -- return the error
    if !worktree_path.exists() {
        return Ok(());
    }
    let result = std::fs::remove_dir(worktree_path);
    if result.is_ok() || !force {
        return result;
    }
    std::fs::remove_dir_all(worktree_path)
}

/// Returns true if a `git worktree remove` stderr indicates the failure was
/// caused by modified or untracked files (i.e., re-running with `--force` would
/// resolve it). Matches the wording git itself uses: "contains modified or
/// untracked files, use --force to delete it".
pub fn is_dirty_worktree_error(error: &str) -> bool {
    let lower = error.to_lowercase();
    lower.contains("modified or untracked files")
        || (lower.contains("--force") && lower.contains("contains"))
}

/// Returns true if a `git worktree` stderr indicates git has no admin entry
/// for the path (`fatal: '<path>' is not a working tree`).
///
/// This is the "already gone from git's point of view" case, not a failure to
/// act on: the checkout can still be on disk with a dangling `.git` pointer
/// (its `gitdir:` target under `.git/worktrees/` was pruned or the repo was
/// re-cloned), so `git worktree remove` refuses while the directory itself is
/// still ours to delete. Callers recover by removing the directory by hand and
/// pruning, the same path a missing `.git` takes.
///
/// Without this classifier the error fell through to the generic arm and the
/// trash auto-purge failed on every hourly sweep forever, never draining the
/// expired session (#3171).
pub fn is_not_a_worktree_error(error: &str) -> bool {
    error.to_lowercase().contains("is not a working tree")
}

/// Enumerate modified, staged, and untracked files inside a worktree using
/// libgit2. Returns a vec of `"<status> <path>"` entries (e.g.
/// `"modified src/foo.rs"`, `"untracked debug.log"`).
///
/// Returns an empty vec if the path is not a git repo or the status walk fails;
/// the caller treats this as "no list available" and falls back to the bare
/// stderr.
pub fn list_dirty_files(worktree_path: &Path) -> Vec<String> {
    let Ok(repo) = open_repo_at(worktree_path) else {
        return Vec::new();
    };

    let mut opts = git2::StatusOptions::new();
    opts.include_untracked(true)
        .recurse_untracked_dirs(true)
        .include_ignored(false);

    let Ok(statuses) = repo.statuses(Some(&mut opts)) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for entry in statuses.iter() {
        let path = entry.path().unwrap_or("<unreadable path>").to_string();
        let label = describe_status(entry.status());
        out.push(format!("{} {}", label, path));
    }
    out
}

fn describe_status(status: git2::Status) -> &'static str {
    if status.contains(git2::Status::CONFLICTED) {
        "conflicted"
    } else if status.intersects(git2::Status::WT_NEW) {
        "untracked"
    } else if status.intersects(git2::Status::INDEX_NEW) {
        "added   "
    } else if status.intersects(git2::Status::WT_DELETED | git2::Status::INDEX_DELETED) {
        "deleted "
    } else if status.intersects(git2::Status::WT_RENAMED | git2::Status::INDEX_RENAMED) {
        "renamed "
    } else if status.intersects(git2::Status::WT_TYPECHANGE | git2::Status::INDEX_TYPECHANGE) {
        "typechg "
    } else if status.intersects(git2::Status::WT_MODIFIED | git2::Status::INDEX_MODIFIED) {
        "modified"
    } else {
        "changed "
    }
}

/// Build a "worktree is dirty" error message for the host-side dirty check
/// in `perform_deletion`. Returns `None` if the worktree has no uncommitted
/// changes. The message is formatted the same way as
/// `enrich_worktree_remove_error`: a short lead line followed by a capped
/// list of dirty paths.
///
/// Used to gate the destructive in-container preclean for sandboxed
/// sessions: the preclean's `find . -delete` wipes the worktree
/// unconditionally, which silently violates the `force_delete=false`
/// contract for users with untracked files. The caller checks this
/// before running preclean, surfaces the message as a deletion error,
/// and skips both preclean and host-side worktree removal for that
/// path.
pub fn dirty_worktree_message(worktree_path: &Path) -> Option<String> {
    let dirty = list_dirty_files(worktree_path);
    if dirty.is_empty() {
        return None;
    }
    let total = dirty.len();
    let mut out = String::with_capacity(96 + total * 32);
    out.push_str("contains modified or untracked files, use --force to delete");
    out.push('\n');
    out.push('\n');
    out.push_str(&format!(
        "Uncommitted changes ({}; force delete will discard these):",
        total
    ));
    for entry in dirty.iter().take(MAX_DIRTY_FILES_LISTED) {
        out.push('\n');
        out.push_str("  ");
        out.push_str(entry);
    }
    if total > MAX_DIRTY_FILES_LISTED {
        out.push('\n');
        out.push_str(&format!(
            "  ... and {} more",
            total - MAX_DIRTY_FILES_LISTED
        ));
    }
    Some(out)
}

/// Build an enriched error message for a failed worktree removal. When the
/// failure is caused by uncommitted/untracked files, list the offending paths
/// (capped at `MAX_DIRTY_FILES_LISTED`) so the user can decide whether
/// re-running with "force delete" is safe.
pub fn enrich_worktree_remove_error(stderr: &str, worktree_path: &Path) -> String {
    if !is_dirty_worktree_error(stderr) {
        return stderr.to_string();
    }

    let dirty = list_dirty_files(worktree_path);
    if dirty.is_empty() {
        return stderr.to_string();
    }

    let total = dirty.len();
    let mut out = String::with_capacity(stderr.len() + 64 + total * 32);
    out.push_str(stderr);
    out.push('\n');
    out.push('\n');
    out.push_str(&format!(
        "Uncommitted changes ({}; force delete will discard these):",
        total
    ));
    for entry in dirty.iter().take(MAX_DIRTY_FILES_LISTED) {
        out.push('\n');
        out.push_str("  ");
        out.push_str(entry);
    }
    if total > MAX_DIRTY_FILES_LISTED {
        out.push('\n');
        out.push_str(&format!(
            "  ... and {} more",
            total - MAX_DIRTY_FILES_LISTED
        ));
    }
    out
}

/// Returns true if a `git worktree move`/`remove` stderr indicates the failure
/// was caused by submodules. Git refuses both whenever the worktree's admin dir
/// still holds `modules/<sub>`, which is where a linked worktree's submodule
/// state lives; orphaning it would corrupt the main repo. Note that the refusal
/// keys on that admin dir alone, so `git submodule deinit` does not lift it:
/// only removing `modules/<sub>` does.
pub fn is_submodule_blocker(error: &str) -> bool {
    let lower = error.to_lowercase();
    lower.contains("working trees containing submodules cannot be moved or removed")
}

/// Resolve a linked worktree's `.git` pointer to its admin dir.
///
/// The target is resolved against the worktree, since aoe rewrites every
/// managed worktree's pointer to a relative path in `create_worktree` and git
/// itself writes one under `worktree.useRelativePaths`.
pub(crate) fn read_linked_worktree_gitdir(worktree_path: &Path) -> Option<PathBuf> {
    let contents = std::fs::read_to_string(worktree_path.join(".git")).ok()?;
    let raw = contents
        .lines()
        .find_map(|line| line.strip_prefix("gitdir:").map(str::trim))?;
    let path = PathBuf::from(raw);
    Some(if path.is_absolute() {
        path
    } else {
        worktree_path.join(path)
    })
}

/// Recover the linked worktree's administrative name from its `.git` pointer.
fn read_linked_worktree_name(worktree_path: &Path) -> Option<String> {
    read_linked_worktree_gitdir(worktree_path)?
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string)
}

/// Manual cleanup fallback for the submodule-blocker error. Removes the
/// per-worktree `modules/` directory git would normally orphan, deletes the
/// worktree checkout, then prunes the now-stale entry from the main repo's
/// worktree list. Equivalent to the three-command shell workaround a user
/// would run by hand. Returns the list of errors encountered; empty means
/// success.
pub fn manual_submodule_worktree_cleanup(
    git_wt: &GitWorktree,
    worktree_path: &Path,
    main_repo: &Path,
) -> Vec<String> {
    let mut errors = Vec::new();

    // This path reaps the admin entry with `prune` (below), which skips locked
    // worktrees; unlock first so an aoe-locked entry is actually removed.
    git_wt.unlock_worktree(worktree_path);

    if let Some(name) = read_linked_worktree_name(worktree_path) {
        let modules_dir = main_repo.join(".git/worktrees").join(&name).join("modules");
        if modules_dir.exists() {
            if let Err(e) = std::fs::remove_dir_all(&modules_dir) {
                tracing::debug!(target: "git.worktree",
                    path = %modules_dir.display(),
                    error = %e,
                    "failed to remove orphaned worktree modules dir"
                );
                errors.push(format!("Submodule cleanup: {}", e));
            } else {
                tracing::debug!(target: "git.worktree",
                    path = %modules_dir.display(),
                    "removed orphaned worktree modules dir"
                );
            }
        }
    }

    if let Err(e) = remove_worktree_dir(worktree_path, main_repo, true) {
        errors.push(format!("Worktree: {}", e));
    }

    if let Err(e) = git_wt.prune_worktrees() {
        errors.push(format!("Worktree: {}", e));
    }

    errors
}

/// Check if a git error message indicates a permission problem.
pub fn is_permission_error(error: &str) -> bool {
    let lower = error.to_lowercase();
    lower.contains("permission denied")
        || lower.contains("operation not permitted")
        || lower.contains("access is denied")
}

/// Delete worktree contents from inside the sandbox container without ever
/// starting its managed entrypoint.
///
/// The outcome distinguishes an absent container from a present stopped or
/// uncleanable one. Callers that intend to destroy the container must fail
/// closed on [`SandboxCleanup::Blocked`], rather than losing the only mounted
/// view of the worktree and then removing its bind mount.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxCleanup {
    Cleaned,
    Absent,
    Blocked,
}

pub fn cleanup_sandbox_worktree(instance: &Instance) -> SandboxCleanup {
    let container = DockerContainer::from_session_id(&instance.id);
    match container.exists() {
        Ok(true) => {}
        Ok(false) => return SandboxCleanup::Absent,
        Err(e) => {
            tracing::warn!(
                target: "containers.runtime",
                session = %instance.id,
                error = %e,
                "container existence probe failed during worktree cleanup; retaining worktree"
            );
            return SandboxCleanup::Blocked;
        }
    }
    match container.probe_running() {
        crate::containers::Probe::Running => {}
        crate::containers::Probe::NotRunning => {
            tracing::warn!(
                target: "containers.runtime",
                session = %instance.id,
                "container is stopped; retaining worktree because in-container cleanup is impossible"
            );
            return SandboxCleanup::Blocked;
        }
        crate::containers::Probe::Unknown(e) => {
            tracing::warn!(
                target: "containers.runtime",
                session = %instance.id,
                error = %e,
                "container running-state probe failed during worktree cleanup; retaining worktree"
            );
            return SandboxCleanup::Blocked;
        }
    }
    match container.exec(&["find", ".", "-mindepth", "1", "-delete"]) {
        Ok(output) if output.status.success() => SandboxCleanup::Cleaned,
        Ok(_) => SandboxCleanup::Blocked,
        Err(_) => SandboxCleanup::Blocked,
    }
}

/// Remove a managed checkout without exceeding the caller's sandbox permissions.
/// Whole-root cleanup and container removal are independent permissions.
pub(crate) fn remove_managed_worktree(
    git_wt: &GitWorktree,
    worktree_path: &Path,
    main_repo: &Path,
    instance: &Instance,
    options: WorktreeCleanupOptions,
    protection: &[&CleanupProtection],
) -> Result<(), Vec<String>> {
    let WorktreeCleanupOptions {
        force,
        allow_container_removal,
        allow_root_cleanup,
    } = options;
    let mut errors = Vec::new();
    let has_dot_git = worktree_path.join(".git").exists();

    tracing::debug!(target: "git.worktree",
        path = %worktree_path.display(),
        has_dot_git,
        is_sandboxed = instance.is_sandboxed(),
        force,
        allow_container_removal,
        "worktree cleanup starting"
    );

    let mut worktree_removed = false;

    if !has_dot_git {
        // Preclean can leave mount-point debris after removing .git.
        // Without permission to preclean, recursive removal still requires force.
        // Unlock orphaned admin entries before pruning.
        git_wt.unlock_worktree(worktree_path);

        let effective_force = force || (instance.is_sandboxed() && allow_root_cleanup);
        match remove_worktree_dir(worktree_path, main_repo, effective_force) {
            Ok(()) => {
                worktree_removed = true;
            }
            Err(e) => {
                tracing::debug!(target: "git.worktree", error = %e, kind = ?e.kind(), "remove_worktree_dir failed (no .git)");
                if is_permission_error(&e.to_string())
                    && try_sandbox_dir_cleanup(
                        worktree_path,
                        main_repo,
                        instance,
                        allow_container_removal,
                        allow_root_cleanup,
                    )
                {
                    worktree_removed = true;
                } else {
                    errors.push(format!("Worktree: {}", e));
                }
            }
        }
        // `prune` is repo-wide but lock-respecting: it reaps entries whose
        // checkout is missing yet SKIPS locked ones, so an aoe-locked worktree
        // whose checkout is invisible from here (a sibling sandbox, a container
        // mount) is never wrongly reaped (#2414). If this session's own locked
        // entry survives here because its stored path diverged from git's
        // registered path, the scoped self-heal in `delete_branch` reaps it by
        // the exact path git reports for this branch.
        if let Err(e) = git_wt.prune_worktrees() {
            errors.push(format!("Worktree: {}", e));
        }
    } else {
        match git_wt.remove_worktree(worktree_path, force) {
            Ok(()) => {
                worktree_removed = true;
            }
            Err(e) => {
                let err_str = e.to_string();
                tracing::debug!(target: "git.worktree",
                    error = %err_str,
                    is_perm = is_permission_error(&err_str),
                    is_submodule = is_submodule_blocker(&err_str),
                    "git worktree remove failed"
                );
                // git has no admin entry for this path, so there is nothing
                // for `git worktree remove` to do and re-running can never
                // succeed. The checkout is still on disk (with a dangling
                // `.git` pointer), so finish the job by hand exactly as the
                // missing-`.git` branch above does. Checked before the
                // permission/submodule fallbacks: those recover a live
                // worktree, and this one is not one. See #3171.
                if read_linked_worktree_gitdir(worktree_path).is_some_and(|path| !path.exists())
                    || is_not_a_worktree_error(&err_str)
                {
                    tracing::info!(target: "git.worktree",
                        path = %worktree_path.display(),
                        "git has no worktree entry for this path; removing the leftover directory by hand"
                    );
                    let effective_force = force || (instance.is_sandboxed() && allow_root_cleanup);
                    match remove_worktree_dir(worktree_path, main_repo, effective_force) {
                        Ok(()) => worktree_removed = true,
                        Err(e2) if is_permission_error(&e2.to_string()) => {
                            if try_sandbox_dir_cleanup(
                                worktree_path,
                                main_repo,
                                instance,
                                allow_container_removal,
                                allow_root_cleanup,
                            ) {
                                worktree_removed = true;
                            } else {
                                errors.push(format!("Worktree: {}", e2));
                            }
                        }
                        Err(e2) => errors.push(format!("Worktree: {}", e2)),
                    }
                    if worktree_removed {
                        if let Err(e2) = git_wt.prune_worktrees() {
                            errors.push(format!("Worktree: {}", e2));
                        }
                    }
                }
                // Container cleanup deletes everything including .git, so
                // git worktree remove won't work afterward. Fall back to
                // removing the directory and pruning stale references.
                else if is_permission_error(&err_str)
                    && try_sandbox_dir_cleanup(
                        worktree_path,
                        main_repo,
                        instance,
                        allow_container_removal,
                        allow_root_cleanup,
                    )
                {
                    worktree_removed = true;
                    if let Err(e2) = git_wt.prune_worktrees() {
                        errors.push(format!("Worktree: {}", e2));
                    }
                } else if is_submodule_blocker(&err_str) {
                    // The only way past this refusal is removing the admin
                    // `modules/<sub>` dir, which is what the manual teardown
                    // does. `git submodule deinit` leaves that dir behind, so
                    // it cannot serve as a pre-step here.
                    let manual_errors =
                        manual_submodule_worktree_cleanup(git_wt, worktree_path, main_repo);
                    if manual_errors.is_empty() {
                        worktree_removed = true;
                    } else {
                        errors.push(format!(
                            "Worktree: {}",
                            enrich_worktree_remove_error(&err_str, worktree_path)
                        ));
                        for me in manual_errors {
                            errors.push(me);
                        }
                    }
                } else {
                    errors.push(format!(
                        "Worktree: {}",
                        enrich_worktree_remove_error(&err_str, worktree_path)
                    ));
                }
            }
        }
    }

    if worktree_removed {
        prune_empty_parent_dirs(worktree_path, main_repo, protection);
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Recover a permission failure only when whole-root cleanup and container removal are allowed.
fn try_sandbox_dir_cleanup(
    worktree_path: &Path,
    main_repo: &Path,
    instance: &Instance,
    allow_container_removal: bool,
    allow_root_cleanup: bool,
) -> bool {
    if !instance.is_sandboxed() {
        return false;
    }
    if !allow_container_removal || !allow_root_cleanup {
        tracing::debug!(target: "git.worktree", "sandbox fallback exceeds caller cleanup permissions");
        return false;
    }

    match cleanup_sandbox_worktree(instance) {
        crate::git::cleanup::SandboxCleanup::Cleaned => {}
        crate::git::cleanup::SandboxCleanup::Absent => {}
        crate::git::cleanup::SandboxCleanup::Blocked => return false,
    }

    let container = DockerContainer::from_session_id(&instance.id);
    if let crate::containers::Teardown::Failed(error) = container.teardown(&instance.id) {
        tracing::debug!(target: "git.worktree", %error, "container removal failed; cleanup retained");
        return false;
    }

    match remove_worktree_dir(worktree_path, main_repo, true) {
        Ok(()) => true,
        Err(e) => {
            tracing::debug!(target: "git.worktree", error = %e, kind = ?e.kind(), "remove_worktree_dir failed after cleanup");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_remove_worktree_dir_refuses_same_as_main_repo() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path();
        let result = remove_worktree_dir(path, path, false);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("refusing to delete"));
        assert!(path.exists());
    }

    #[test]
    fn test_remove_worktree_dir_removes_empty_dir() {
        let dir = tempfile::TempDir::new().unwrap();
        let main = dir.path().join("main");
        let wt = dir.path().join("worktree");
        std::fs::create_dir(&main).unwrap();
        std::fs::create_dir(&wt).unwrap();
        let result = remove_worktree_dir(&wt, &main, false);
        assert!(result.is_ok());
        assert!(!wt.exists());
    }

    // Missing `.git` permits recursive sandbox cleanup only for an unshared root.
    #[test]
    fn test_remove_managed_worktree_sandboxed_clears_only_unshared_root_cruft() {
        use crate::session::{Instance, SandboxInfo};

        for allow_root_cleanup in [false, true] {
            let tmp = tempfile::TempDir::new().unwrap();
            let main_repo = tmp.path().join("main");
            let worktree_path = tmp.path().join("worktree");
            std::fs::create_dir(&main_repo).unwrap();

            let repo = git2::Repository::init(&main_repo).unwrap();
            let sig = git2::Signature::now("Test", "test@example.com").unwrap();
            let tree_id = repo.index().unwrap().write_tree().unwrap();
            let tree = repo.find_tree(tree_id).unwrap();
            repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
                .unwrap();

            let status = std::process::Command::new("git")
                .args([
                    "worktree",
                    "add",
                    "-b",
                    "feature/cruft",
                    worktree_path.to_str().unwrap(),
                ])
                .current_dir(&main_repo)
                .output()
                .unwrap();
            assert!(
                status.status.success(),
                "git worktree add failed: {}",
                String::from_utf8_lossy(&status.stderr)
            );

            // Model mount-point leftovers after the in-container preclean.
            std::fs::remove_file(worktree_path.join(".git")).unwrap();
            std::fs::create_dir(worktree_path.join("target")).unwrap();
            std::fs::create_dir(worktree_path.join("node_modules")).unwrap();
            std::fs::create_dir(worktree_path.join(".venv")).unwrap();

            let mut instance = Instance::new("Test", worktree_path.to_str().unwrap());
            instance.sandbox_info = Some(SandboxInfo {
                enabled: true,
                container_id: None,
                image: "alpine".to_string(),
                container_name: "aoe-cruft-doesnotexist".to_string(),
                extra_env: None,
                custom_instruction: None,
                before_start_env: Vec::new(),
                container_workdir: None,
            });

            let git_wt = GitWorktree::new(main_repo.clone()).unwrap();
            let result = remove_managed_worktree(
                &git_wt,
                &worktree_path,
                &main_repo,
                &instance,
                WorktreeCleanupOptions {
                    force: false,
                    allow_container_removal: true,
                    allow_root_cleanup,
                },
                &[&CleanupProtection::default()],
            );

            if allow_root_cleanup {
                assert!(result.is_ok(), "sandbox cleanup failed: {result:?}");
                assert!(!worktree_path.exists());
            } else {
                assert!(result.is_err());
                assert!(worktree_path.join("target").is_dir());
                assert!(worktree_path.join("node_modules").is_dir());
                assert!(worktree_path.join(".venv").is_dir());
            }
        }
    }

    /// Counterpart: non-sandboxed sessions with a missing `.git` get
    /// the strict behavior. A leftover non-empty dir there usually
    /// means the user did something manual (moved files in, partial
    /// recovery), and silently nuking it would be a regression.
    #[test]
    fn test_remove_managed_worktree_non_sandboxed_preserves_strict_dir_check() {
        use crate::session::Instance;

        let tmp = tempfile::TempDir::new().unwrap();
        let main_repo = tmp.path().join("main");
        let worktree_path = tmp.path().join("worktree");
        std::fs::create_dir(&main_repo).unwrap();

        let repo = git2::Repository::init(&main_repo).unwrap();
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        let tree_id = repo.index().unwrap().write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();

        let status = std::process::Command::new("git")
            .args([
                "worktree",
                "add",
                "-b",
                "feature/no-sandbox-cruft",
                worktree_path.to_str().unwrap(),
            ])
            .current_dir(&main_repo)
            .output()
            .unwrap();
        assert!(status.status.success());

        std::fs::remove_file(worktree_path.join(".git")).unwrap();
        std::fs::create_dir(worktree_path.join("target")).unwrap();

        // No sandbox_info: instance.is_sandboxed() is false.
        let instance = Instance::new("Test", worktree_path.to_str().unwrap());

        let git_wt = GitWorktree::new(main_repo.clone()).unwrap();
        let result = remove_managed_worktree(
            &git_wt,
            &worktree_path,
            &main_repo,
            &instance,
            WorktreeCleanupOptions {
                force: false,
                allow_container_removal: false,
                allow_root_cleanup: true,
            },
            &[&CleanupProtection::default()],
        );

        assert!(
            result.is_err(),
            "non-sandboxed removal must NOT silently force-clear leftover dirs"
        );
        assert!(
            worktree_path.exists(),
            "worktree dir should still exist after strict failure"
        );
    }

    #[test]
    fn test_is_permission_error_matches() {
        assert!(is_permission_error("Permission denied (os error 13)"));
        assert!(is_permission_error("operation not permitted"));
        assert!(is_permission_error("Access is denied"));
        assert!(!is_permission_error("file not found"));
    }

    #[test]
    fn test_is_submodule_blocker_matches_git_message() {
        assert!(is_submodule_blocker(
            "fatal: working trees containing submodules cannot be moved or removed"
        ));
        assert!(is_submodule_blocker(
            "Git worktree command failed: fatal: working trees containing submodules cannot be moved or removed"
        ));
        assert!(!is_submodule_blocker("permission denied"));
        assert!(!is_submodule_blocker(
            "contains modified or untracked files"
        ));
    }

    #[test]
    fn test_read_linked_worktree_name_parses_gitdir_line() {
        let dir = tempfile::TempDir::new().unwrap();
        let wt = dir.path().join("wt");
        std::fs::create_dir(&wt).unwrap();
        std::fs::write(
            wt.join(".git"),
            "gitdir: /tmp/main/.git/worktrees/feature-foo\n",
        )
        .unwrap();
        assert_eq!(
            read_linked_worktree_name(&wt),
            Some("feature-foo".to_string())
        );
    }

    #[test]
    fn test_read_linked_worktree_name_returns_none_without_dotgit() {
        let dir = tempfile::TempDir::new().unwrap();
        assert!(read_linked_worktree_name(dir.path()).is_none());
    }

    #[test]
    fn test_manual_submodule_worktree_cleanup_removes_modules_dir() {
        // Build a main repo + a linked-worktree layout by hand: main repo has
        // `.git/worktrees/feature-foo/modules/<sub>` (the orphaned submodule
        // state git refuses to leave behind), and the worktree has a `.git`
        // file pointing back to that entry. The manual fallback should clear
        // the modules dir, the worktree checkout, and prune the stale entry.
        let dir = tempfile::TempDir::new().unwrap();
        let main_repo = dir.path().join("main");
        std::fs::create_dir_all(&main_repo).unwrap();
        let repo = git2::Repository::init(&main_repo).unwrap();
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        let tree_id = repo.index().unwrap().write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();

        let modules_dir = main_repo.join(".git/worktrees/feature-foo/modules/sub");
        std::fs::create_dir_all(&modules_dir).unwrap();
        std::fs::write(modules_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();

        let wt = dir.path().join("feature-foo");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(
            wt.join(".git"),
            format!(
                "gitdir: {}\n",
                main_repo.join(".git/worktrees/feature-foo").display()
            ),
        )
        .unwrap();

        let git_wt = GitWorktree::new(main_repo.clone()).unwrap();
        let errors = manual_submodule_worktree_cleanup(&git_wt, &wt, &main_repo);

        assert!(
            errors.is_empty(),
            "expected clean cleanup, got: {:?}",
            errors
        );
        assert!(!modules_dir.exists(), "modules dir should be removed");
        assert!(!wt.exists(), "worktree dir should be removed");
    }

    #[test]
    fn test_is_dirty_worktree_error_matches_git_message() {
        assert!(is_dirty_worktree_error(
            "fatal: '/tmp/wt' contains modified or untracked files, use --force to delete it"
        ));
        assert!(!is_dirty_worktree_error("permission denied"));
        assert!(!is_dirty_worktree_error("file not found"));
    }

    #[test]
    fn test_is_not_a_worktree_error_matches_git_message() {
        assert!(is_not_a_worktree_error(
            "fatal: '/tmp/wt/.aoe-trash/abc' is not a working tree"
        ));
        // The wrapped form callers actually see through GitError.
        assert!(is_not_a_worktree_error(
            "Git worktree command failed: fatal: '/tmp/wt' is not a working tree"
        ));
        assert!(!is_not_a_worktree_error("permission denied"));
        assert!(!is_not_a_worktree_error(
            "fatal: '/tmp/wt' contains modified or untracked files"
        ));
    }

    /// A trashed worktree whose git admin entry went missing (pruned, or the
    /// main repo re-cloned) keeps its checkout and a now-dangling `.git`
    /// pointer on disk. `git worktree remove` refuses with "is not a working
    /// tree", which is unfixable by retrying: before #3171 that error fell
    /// through to the generic arm, so `remove_managed_worktree` failed and
    /// the hourly trash auto-purge re-failed on the same session forever
    /// (observed retrying every hour with no progress). The recovery is to
    /// delete the leftover directory by hand and prune, the same as a
    /// missing `.git`.
    #[test]
    fn test_remove_managed_worktree_recovers_from_missing_admin_entry() {
        use crate::session::Instance;

        let tmp = tempfile::TempDir::new().unwrap();
        let main_repo = tmp.path().join("main");
        let worktree_path = tmp.path().join("trashed");
        std::fs::create_dir(&main_repo).unwrap();

        let repo = git2::Repository::init(&main_repo).unwrap();
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        let tree_id = repo.index().unwrap().write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();

        let status = std::process::Command::new("git")
            .args([
                "worktree",
                "add",
                "-b",
                "feature/orphaned-admin-entry",
                worktree_path.to_str().unwrap(),
            ])
            .current_dir(&main_repo)
            .output()
            .unwrap();
        assert!(
            status.status.success(),
            "git worktree add failed: {}",
            String::from_utf8_lossy(&status.stderr)
        );

        // Orphan the checkout: drop git's admin entry while leaving the
        // worktree (and its `.git` pointer file) on disk. This is the exact
        // state that makes both `worktree unlock` and `worktree remove`
        // report "is not a working tree".
        let admin = main_repo.join(".git/worktrees");
        std::fs::remove_dir_all(&admin).unwrap();
        assert!(
            worktree_path.join(".git").exists(),
            "test must exercise the has_dot_git branch"
        );

        let instance = Instance::new("Test", worktree_path.to_str().unwrap());
        let git_wt = GitWorktree::new(main_repo.clone()).unwrap();

        // force=true mirrors the auto-purge, which forces removal so a dirty
        // tree can't pin an expired session in the trash forever.
        let result = remove_managed_worktree(
            &git_wt,
            &worktree_path,
            &main_repo,
            &instance,
            WorktreeCleanupOptions {
                force: true,
                allow_container_removal: false,
                allow_root_cleanup: true,
            },
            &[&CleanupProtection::default()],
        );

        assert!(
            result.is_ok(),
            "orphaned admin entry must not fail removal: {:?}",
            result
        );
        assert!(
            !worktree_path.exists(),
            "leftover worktree dir should be gone"
        );

        // Idempotent: the purge may re-run before its registry entry drains.
        let again = remove_managed_worktree(
            &git_wt,
            &worktree_path,
            &main_repo,
            &instance,
            WorktreeCleanupOptions {
                force: true,
                allow_container_removal: false,
                allow_root_cleanup: true,
            },
            &[&CleanupProtection::default()],
        );
        assert!(again.is_ok(), "second removal must be a no-op: {:?}", again);
    }

    fn init_repo_with_commit() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::TempDir::new().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        let tree_id = {
            let mut index = repo.index().unwrap();
            index.write_tree().unwrap()
        };
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();
        let path = dir.path().to_path_buf();
        (dir, path)
    }

    #[test]
    fn test_list_dirty_files_returns_untracked_and_modified() {
        let (_dir, repo_path) = init_repo_with_commit();

        // Untracked file
        std::fs::write(repo_path.join("new.txt"), "hello").unwrap();

        // Tracked + modified file: commit it first, then modify.
        std::fs::write(repo_path.join("tracked.txt"), "v1").unwrap();
        let repo = git2::Repository::open(&repo_path).unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new("tracked.txt")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        let parent = repo.head().unwrap().peel_to_commit().unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "add tracked", &tree, &[&parent])
            .unwrap();
        std::fs::write(repo_path.join("tracked.txt"), "v2-modified").unwrap();

        let dirty = list_dirty_files(&repo_path);
        assert!(
            dirty.iter().any(|s| s.contains("new.txt")),
            "expected untracked new.txt in {:?}",
            dirty
        );
        assert!(
            dirty.iter().any(|s| s.contains("tracked.txt")),
            "expected modified tracked.txt in {:?}",
            dirty
        );
        assert!(dirty.iter().any(|s| s.starts_with("untracked ")));
        assert!(dirty.iter().any(|s| s.starts_with("modified ")));
    }

    #[test]
    fn test_list_dirty_files_returns_empty_for_non_repo() {
        let dir = tempfile::TempDir::new().unwrap();
        assert!(list_dirty_files(dir.path()).is_empty());
    }

    #[test]
    fn test_dirty_worktree_message_some_when_untracked() {
        let (_dir, repo_path) = init_repo_with_commit();
        std::fs::write(repo_path.join("scratch.log"), "data").unwrap();

        let msg =
            dirty_worktree_message(&repo_path).expect("dirty worktree should produce message");
        assert!(
            msg.contains("modified or untracked files"),
            "message should describe dirty state: {}",
            msg
        );
        assert!(
            msg.contains("--force"),
            "message should mention --force: {}",
            msg
        );
        assert!(
            msg.contains("scratch.log"),
            "message should list the dirty path: {}",
            msg
        );
    }

    #[test]
    fn test_dirty_worktree_message_none_when_clean() {
        let (_dir, repo_path) = init_repo_with_commit();
        assert!(
            dirty_worktree_message(&repo_path).is_none(),
            "clean worktree should produce no message"
        );
    }

    #[test]
    fn test_enrich_worktree_remove_error_appends_file_list() {
        let (_dir, repo_path) = init_repo_with_commit();
        std::fs::write(repo_path.join("scratch.log"), "data").unwrap();

        let stderr =
            "fatal: '/some/path' contains modified or untracked files, use --force to delete it";
        let enriched = enrich_worktree_remove_error(stderr, &repo_path);

        assert!(enriched.contains(stderr));
        assert!(enriched.contains("Uncommitted changes"));
        assert!(enriched.contains("scratch.log"));
    }

    #[test]
    fn test_enrich_worktree_remove_error_passes_through_unrelated_errors() {
        let (_dir, repo_path) = init_repo_with_commit();
        std::fs::write(repo_path.join("scratch.log"), "data").unwrap();

        let stderr = "fatal: permission denied";
        let enriched = enrich_worktree_remove_error(stderr, &repo_path);
        assert_eq!(enriched, stderr);
    }

    #[test]
    fn test_enrich_worktree_remove_error_caps_long_lists() {
        let (_dir, repo_path) = init_repo_with_commit();
        for i in 0..(MAX_DIRTY_FILES_LISTED + 5) {
            std::fs::write(repo_path.join(format!("f{}.txt", i)), "x").unwrap();
        }
        let stderr =
            "fatal: '/some/path' contains modified or untracked files, use --force to delete it";
        let enriched = enrich_worktree_remove_error(stderr, &repo_path);
        assert!(enriched.contains("and 5 more"));
    }

    /// Mirrors the user's nested template `../{repo-name}-worktrees/{branch}/{repo-name}`
    /// where the worktree leaf is two levels below a `<repo>-worktrees` base.
    /// After removing the leaf, both intermediate dirs should also be cleaned.
    #[test]
    fn test_prune_empty_parent_dirs_climbs_through_nested_template() {
        let dir = tempfile::TempDir::new().unwrap();
        let main_repo = dir.path().join("clawbolt-premium");
        let base = dir.path().join("clawbolt-premium-worktrees");
        let branch_dir = base.join("feature-foo");
        let worktree = branch_dir.join("clawbolt-premium");
        std::fs::create_dir_all(&main_repo).unwrap();
        std::fs::create_dir_all(&worktree).unwrap();

        // Simulate the leaf having just been removed by `git worktree remove`.
        std::fs::remove_dir(&worktree).unwrap();
        assert!(branch_dir.exists());

        prune_empty_parent_dirs(&worktree, &main_repo, &[&CleanupProtection::default()]);

        assert!(!branch_dir.exists(), "branch wrapper dir should be gone");
        assert!(!base.exists(), "worktrees base dir should be gone");
        assert!(main_repo.exists(), "main repo must be untouched");
    }

    /// `on_create` hooks sometimes drop a sibling repo next to the worktree
    /// (e.g. an OSS pin clone). After deleting the worktree, that sibling
    /// keeps the wrapper non-empty and we MUST leave it alone.
    #[test]
    fn test_prune_empty_parent_dirs_preserves_non_empty_wrapper() {
        let dir = tempfile::TempDir::new().unwrap();
        let main_repo = dir.path().join("clawbolt-premium");
        let base = dir.path().join("clawbolt-premium-worktrees");
        let branch_dir = base.join("feature-foo");
        let worktree = branch_dir.join("clawbolt-premium");
        let sibling = branch_dir.join("clawbolt"); // orphan from on_create hook
        std::fs::create_dir_all(&main_repo).unwrap();
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        std::fs::write(sibling.join("README.md"), "oss pin").unwrap();

        std::fs::remove_dir_all(&worktree).unwrap();

        prune_empty_parent_dirs(&worktree, &main_repo, &[&CleanupProtection::default()]);

        assert!(
            branch_dir.exists(),
            "wrapper must survive non-empty sibling"
        );
        assert!(sibling.exists(), "sibling repo must not be touched");
    }

    /// Default template `../{repo-name}-worktrees/{branch}` keeps the
    /// `<repo>-worktrees` base shared across multiple sessions. If another
    /// branch's worktree is still there, we must stop at the base.
    #[test]
    fn test_prune_empty_parent_dirs_stops_at_shared_base() {
        let dir = tempfile::TempDir::new().unwrap();
        let main_repo = dir.path().join("clawbolt-premium");
        let base = dir.path().join("clawbolt-premium-worktrees");
        let deleted_wt = base.join("feature-foo");
        let other_wt = base.join("feature-bar");
        std::fs::create_dir_all(&main_repo).unwrap();
        std::fs::create_dir_all(&deleted_wt).unwrap();
        std::fs::create_dir_all(&other_wt).unwrap();

        std::fs::remove_dir(&deleted_wt).unwrap();

        prune_empty_parent_dirs(&deleted_wt, &main_repo, &[&CleanupProtection::default()]);

        assert!(base.exists(), "shared base must survive other worktrees");
        assert!(other_wt.exists(), "other worktree must be untouched");
    }

    #[cfg(unix)]
    #[test]
    fn parent_pruning_preserves_the_survivors_symlink_traversal() {
        let root = tempfile::tempdir().unwrap();
        let main_repo = root.path().join("main");
        let real = root.path().join("real");
        let alias = root.path().join("alias");
        for path in [&main_repo, &real.join("empty"), &real.join("peer")] {
            std::fs::create_dir_all(path).unwrap();
        }
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let owner = Instance::new("peer", alias.join("empty/../peer").to_str().unwrap());
        let protection = CleanupProtection::new([&owner]).unwrap();
        prune_empty_parent_dirs(&alias.join("empty/deleted"), &main_repo, &[&protection]);
        assert!(
            std::path::Path::new(&owner.project_path).is_dir(),
            "pruning broke the surviving session cwd"
        );
    }

    /// Bare-repo template `./{branch}` puts the worktree inside the main repo.
    /// We must never remove the main repo or any of its ancestors.
    #[test]
    fn test_prune_empty_parent_dirs_refuses_to_climb_into_main_repo() {
        let dir = tempfile::TempDir::new().unwrap();
        let main_repo = dir.path().join("bare-repo");
        let worktree = main_repo.join("feature-foo");
        std::fs::create_dir_all(&worktree).unwrap();

        std::fs::remove_dir(&worktree).unwrap();

        prune_empty_parent_dirs(&worktree, &main_repo, &[&CleanupProtection::default()]);

        assert!(main_repo.exists(), "main repo must be untouched");
    }

    /// If the wrapper isn't actually empty for any reason (race, leftover
    /// metadata file, FS quirk), `remove_dir` returns ENOTEMPTY and we stop.
    /// Don't ever fall through to recursive deletion.
    #[test]
    fn test_prune_empty_parent_dirs_never_recurses() {
        let dir = tempfile::TempDir::new().unwrap();
        let main_repo = dir.path().join("repo");
        let wrapper = dir.path().join("wrapper");
        let worktree = wrapper.join("wt");
        let stray = wrapper.join("DS_Store_or_similar");
        std::fs::create_dir_all(&main_repo).unwrap();
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(&stray, "junk").unwrap();

        std::fs::remove_dir(&worktree).unwrap();

        prune_empty_parent_dirs(&worktree, &main_repo, &[&CleanupProtection::default()]);

        assert!(wrapper.exists(), "wrapper with stray file must survive");
        assert!(stray.exists(), "stray file must not be touched");
    }
}
