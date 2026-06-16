//! Shared worktree cleanup utilities used by both CLI and TUI deletion paths.

use std::path::Path;

use crate::containers::DockerContainer;
use crate::session::Instance;

use super::open_repo_at;
use super::GitWorktree;

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
fn prune_empty_parent_dirs(worktree_path: &Path, main_repo: &Path) {
    let main_canonical = main_repo
        .canonicalize()
        .unwrap_or_else(|_| main_repo.to_path_buf());
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

        let parent_canonical = parent
            .canonicalize()
            .unwrap_or_else(|_| parent.to_path_buf());

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

/// Returns true if a `git worktree remove` stderr indicates the failure was
/// caused by initialised submodules. Git refuses to remove a worktree whose
/// checkout still has live submodule entries, even with `--force`; submodule
/// state lives under `.git/worktrees/<name>/modules/` and orphaning it would
/// corrupt the main repo.
pub fn is_submodule_blocker(error: &str) -> bool {
    let lower = error.to_lowercase();
    lower.contains("working trees containing submodules cannot be moved or removed")
}

/// Deinitialise any submodules under `worktree_path` so `git worktree remove`
/// will let the worktree go. Best-effort and idempotent: a no-op when the
/// worktree has no `.gitmodules`, no initialised submodules, or git itself
/// isn't reachable. The `-f` on `submodule deinit` is "force-deinit modified
/// submodules"; distinct from aoe's worktree-level `force`, which means
/// "discard uncommitted/untracked files in the worktree itself"; so applying
/// it here doesn't conflate the two semantics.
pub fn deinit_submodules_if_present(worktree_path: &Path) {
    if !worktree_path.join(".gitmodules").exists() {
        return;
    }
    let output = super::command::run_git(worktree_path, ["submodule", "deinit", "-f", "--all"]);
    match output {
        Ok(o) if o.status.success() => {
            tracing::debug!(target: "git.worktree",
                path = %worktree_path.display(),
                "deinitialised submodules before worktree removal"
            );
        }
        Ok(o) => {
            tracing::debug!(target: "git.worktree",
                path = %worktree_path.display(),
                stderr = %String::from_utf8_lossy(&o.stderr),
                "submodule deinit returned non-zero; continuing"
            );
        }
        Err(e) => {
            tracing::debug!(target: "git.worktree",
                path = %worktree_path.display(),
                error = %e,
                "submodule deinit failed to spawn; continuing"
            );
        }
    }
}

/// Read `.git` (file form) in a worktree to recover the linked worktree's
/// administrative name. Returns the basename of the gitdir path, e.g.
/// `<main_repo>/.git/worktrees/feature-foo` → `feature-foo`. None when the
/// `.git` file is missing or doesn't carry a `gitdir:` line.
fn read_linked_worktree_name(worktree_path: &Path) -> Option<String> {
    let dotgit = worktree_path.join(".git");
    let contents = std::fs::read_to_string(&dotgit).ok()?;
    for line in contents.lines() {
        if let Some(rest) = line.strip_prefix("gitdir:") {
            let gitdir = Path::new(rest.trim());
            return gitdir
                .file_name()
                .and_then(|s| s.to_str())
                .map(|s| s.to_string());
        }
    }
    None
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

/// Delete worktree contents from inside the sandbox container.
///
/// Starts the container if it exists but is stopped, then runs
/// `find . -mindepth 1 -delete` to remove all contents (including
/// root-owned files that the host user cannot delete directly).
///
/// Returns true if the container successfully deleted the contents.
pub fn cleanup_sandbox_worktree(instance: &Instance) -> bool {
    let container = DockerContainer::from_session_id(&instance.id);
    if !container.exists().unwrap_or(false) {
        return false;
    }
    if !container.is_running().unwrap_or(false) && container.start().is_err() {
        return false;
    }
    match container.exec(&["find", ".", "-mindepth", "1", "-delete"]) {
        Ok(output) => output.status.success(),
        Err(_) => false,
    }
}

/// Perform full worktree cleanup with automatic sandbox fallback.
///
/// Handles both cases:
/// - `.git` file missing: removes directory and prunes stale references
/// - `.git` file present: uses `git worktree remove`, falls back to
///   container cleanup for sandboxed sessions with permission errors
///
/// `allow_container_removal` controls whether the sandbox fallback is
/// permitted to force-remove the container as part of its recovery.
/// Set to `true` when the caller has already requested container
/// deletion (or is about to), and `false` when the caller wants to
/// preserve the container (e.g. `aoe remove --keep-container` or
/// `delete_worktree=true, delete_sandbox=false`). When `false` and
/// the fallback is the only way to make progress, the worktree
/// removal fails with the original permission error instead of
/// quietly tearing down the container behind the user's back.
///
/// Returns `Ok(())` if the worktree was successfully removed, or
/// `Err(errors)` with error messages on failure.
pub fn remove_managed_worktree(
    git_wt: &GitWorktree,
    worktree_path: &Path,
    main_repo: &Path,
    instance: &Instance,
    force: bool,
    allow_container_removal: bool,
) -> Result<(), Vec<String>> {
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
        // .git is missing (manual deletion or other issue).
        // Remove the dir ourselves and prune stale references.
        //
        // For sandboxed sessions, missing `.git` almost always means the
        // in-container preclean (`find . -delete`) wiped the worktree
        // along with `.git` itself. The only remaining content on the
        // host is mount-point cruft from anonymous volumes (empty
        // `target/`, `node_modules/`, `.venv/` dirs created by Docker
        // as anchors for `-v /workspace/<repo>/target` style mounts).
        // Strict `remove_dir` then fails with ENOTEMPTY ("Directory not
        // empty (os error 66)" on macOS); escalate to `remove_dir_all`
        // so the leftover empty mount-point dirs are cleaned up. The
        // host-side dirty check in `perform_deletion` guarantees we
        // only reach this code path when the user opted in to losing
        // any uncommitted changes (via `force_delete=true`) or the
        // worktree was clean.
        //
        // For non-sandboxed sessions, missing `.git` typically means
        // the user did something manual; keep strict behavior gated on
        // the explicit `force` flag.
        let effective_force = force || instance.is_sandboxed();
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
                    )
                {
                    worktree_removed = true;
                } else {
                    errors.push(format!("Worktree: {}", e));
                }
            }
        }
        if let Err(e) = git_wt.prune_worktrees() {
            errors.push(format!("Worktree: {}", e));
        }
    } else {
        // Submodules are a normal repo state, not a destructive override;
        // `git worktree remove` refuses to delete a worktree with live
        // submodules even with --force, so deinit them ourselves before
        // asking git to remove the checkout. No-op when the worktree has no
        // .gitmodules.
        deinit_submodules_if_present(worktree_path);

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
                // Container cleanup deletes everything including .git, so
                // git worktree remove won't work afterward. Fall back to
                // removing the directory and pruning stale references.
                if is_permission_error(&err_str)
                    && try_sandbox_dir_cleanup(
                        worktree_path,
                        main_repo,
                        instance,
                        allow_container_removal,
                    )
                {
                    worktree_removed = true;
                    if let Err(e2) = git_wt.prune_worktrees() {
                        errors.push(format!("Worktree: {}", e2));
                    }
                } else if is_submodule_blocker(&err_str) {
                    // Pre-deinit didn't fully resolve it (e.g. a broken
                    // submodule), or the worktree carries orphaned modules
                    // state without a live `.gitmodules`. Fall back to manual
                    // teardown.
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

    // Clean up empty wrapper directories created by nested path templates
    // (e.g., `../{repo-name}-worktrees/{branch}/{repo-name}` leaves an empty
    // `{branch}/` behind once the leaf is gone). Best-effort, never fails
    // deletion.
    if worktree_removed {
        prune_empty_parent_dirs(worktree_path, main_repo);
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Try to clean up a worktree directory using the sandbox container.
///
/// When worktree files are root-owned (from container execution), the host
/// can't delete them directly. This function:
/// 1. Runs `find . -mindepth 1 -delete` inside the container
/// 2. Force-removes the container to release the bind mount
/// 3. Retries directory removal (with VirtioFS delay handling)
///
/// Step 2 is gated on `allow_container_removal`: when the caller
/// opted out of container deletion, we refuse to nuke the container
/// just to free a permission-bound worktree. In that case the caller
/// sees the original Worktree permission error and can decide what
/// to do.
fn try_sandbox_dir_cleanup(
    worktree_path: &Path,
    main_repo: &Path,
    instance: &Instance,
    allow_container_removal: bool,
) -> bool {
    if !instance.is_sandboxed() {
        return false;
    }
    if !allow_container_removal {
        tracing::debug!(target: "git.worktree", "sandbox fallback skipped: caller forbade container removal");
        return false;
    }

    let cleaned = cleanup_sandbox_worktree(instance);
    tracing::debug!(target: "git.worktree", cleaned, "container cleanup attempted");
    if !cleaned {
        return false;
    }

    let container = DockerContainer::from_session_id(&instance.id);
    let rm_result = container.remove(true);
    tracing::debug!(target: "git.worktree", ?rm_result, "container force-removed");
    container.remove_named_ignore_volumes(&instance.id);

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

    /// `try_sandbox_dir_cleanup` must respect `allow_container_removal=false`
    /// by returning early without touching the container, even when the
    /// instance is sandboxed and the worktree would otherwise be a
    /// fallback candidate. We can't easily test the docker-side branch
    /// without a real container runtime, but we CAN guarantee the
    /// early-return: a non-sandboxed instance should also return false,
    /// and a sandboxed instance with `allow_container_removal=false`
    /// should not even attempt to invoke `cleanup_sandbox_worktree`.
    /// This regression test is checked via a side effect: we point the
    /// instance at a non-existent worktree path, so the only way the
    /// function could reach the post-cleanup `remove_worktree_dir` call
    /// is if it bypassed the early-return. If the flag is honored,
    /// the function returns false immediately.
    #[test]
    fn test_try_sandbox_dir_cleanup_respects_allow_container_removal_false() {
        use crate::session::{Instance, SandboxInfo};
        let mut instance = Instance::new("Test", "/tmp/aoe-cleanup-test-nonexistent");
        instance.sandbox_info = Some(SandboxInfo {
            enabled: true,
            container_id: None,
            image: "alpine".to_string(),
            container_name: "aoe-sandbox-doesnotexist".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
        });

        let worktree = std::path::PathBuf::from("/tmp/aoe-cleanup-test-nonexistent");
        let main_repo = std::path::PathBuf::from("/tmp/aoe-cleanup-test-main-nonexistent");

        // With allow_container_removal=false, must return false without
        // touching anything.
        let result = try_sandbox_dir_cleanup(&worktree, &main_repo, &instance, false);
        assert!(
            !result,
            "sandbox fallback must bail when allow_container_removal=false"
        );
    }

    /// Anonymous-volume mount-point cruft: when a sandboxed session's
    /// in-container preclean (`find . -delete`) runs, the bind mount on
    /// the host loses its real contents (including `.git`) but Docker
    /// leaves the anonymous-volume mount-point directories behind as
    /// empty `target/`, `node_modules/`, `.venv/` dirs. Strict
    /// `remove_dir` then fails with ENOTEMPTY ("Directory not empty
    /// (os error 66)" on macOS) even though the user opted into
    /// destroying the worktree. `remove_managed_worktree` must escalate
    /// to `remove_dir_all` for sandboxed instances in this case.
    #[test]
    fn test_remove_managed_worktree_sandboxed_clears_mount_point_cruft() {
        use crate::session::{Instance, SandboxInfo};

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

        // Simulate post-preclean state: `.git` was wiped along with
        // everything else, only the anonymous-volume mount-point dirs
        // remain on the host as empty directories.
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
        });

        let git_wt = GitWorktree::new(main_repo.clone()).unwrap();
        let result = remove_managed_worktree(
            &git_wt,
            &worktree_path,
            &main_repo,
            &instance,
            false, // force = false; sandboxed escalation must kick in
            true,  // allow_container_removal (not exercised here)
        );

        assert!(
            result.is_ok(),
            "sandboxed removal must clear mount-point cruft: {:?}",
            result
        );
        assert!(
            !worktree_path.exists(),
            "worktree dir should be gone after sandboxed cleanup"
        );
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
        let result =
            remove_managed_worktree(&git_wt, &worktree_path, &main_repo, &instance, false, false);

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
    fn test_try_sandbox_dir_cleanup_returns_false_for_non_sandboxed() {
        use crate::session::Instance;
        let instance = Instance::new("Test", "/tmp/aoe-cleanup-test-nonexistent");
        // No sandbox_info set.
        let worktree = std::path::PathBuf::from("/tmp/aoe-cleanup-test-nonexistent");
        let main_repo = std::path::PathBuf::from("/tmp/aoe-cleanup-test-main-nonexistent");

        // Even with allow_container_removal=true, a non-sandboxed
        // instance must early-return.
        let result = try_sandbox_dir_cleanup(&worktree, &main_repo, &instance, true);
        assert!(!result);
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
    fn test_deinit_submodules_if_present_is_noop_without_gitmodules() {
        // No `.gitmodules` → function returns without spawning git. We can't
        // observe the no-spawn directly, but the call must not panic and the
        // directory contents must be unchanged.
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("placeholder"), "x").unwrap();
        deinit_submodules_if_present(dir.path());
        assert!(dir.path().join("placeholder").exists());
    }

    #[test]
    fn test_is_dirty_worktree_error_matches_git_message() {
        assert!(is_dirty_worktree_error(
            "fatal: '/tmp/wt' contains modified or untracked files, use --force to delete it"
        ));
        assert!(!is_dirty_worktree_error("permission denied"));
        assert!(!is_dirty_worktree_error("file not found"));
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

        prune_empty_parent_dirs(&worktree, &main_repo);

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

        prune_empty_parent_dirs(&worktree, &main_repo);

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

        prune_empty_parent_dirs(&deleted_wt, &main_repo);

        assert!(base.exists(), "shared base must survive other worktrees");
        assert!(other_wt.exists(), "other worktree must be untouched");
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

        prune_empty_parent_dirs(&worktree, &main_repo);

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

        prune_empty_parent_dirs(&worktree, &main_repo);

        assert!(wrapper.exists(), "wrapper with stray file must survive");
        assert!(stray.exists(), "stray file must not be touched");
    }
}
