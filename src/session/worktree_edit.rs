//! Post-create editing of a managed worktree session's workdir name.
//!
//! A session created in worktree mode bakes its directory name (and,
//! optionally, its branch) at creation time. This module performs the
//! in-place edit the user asks for later: move the worktree directory to a
//! new leaf name and, when opted in, rename the underlying git branch.
//!
//! Design notes (see #1723):
//!   - The new directory is a *sibling-leaf* rename: we keep the existing
//!     parent directory and only swap the final path component. We do NOT
//!     recompute the path from the current config template, because the
//!     random session-id seed used at creation is unrecoverable and the
//!     template may have drifted since, either of which would silently
//!     relocate the session somewhere unexpected.
//!   - Branch rename is opt-in. A session may have already done meaningful
//!     work on its branch (commits, an upstream), so renaming the branch is
//!     a separate, explicit choice from renaming the workdir directory.
//!   - Ordering is branch-rename first, then `git worktree move`. The
//!     filesystem move is the more failure-prone step (open handles, locks),
//!     so it goes last where a best-effort rollback of the branch rename is
//!     a cheap ref operation.

use std::path::{Path, PathBuf};

use crate::containers::{DockerContainer, Probe, Teardown};
use crate::git::error::GitError;
use crate::git::template::sanitize_branch_name;
use crate::git::GitWorktree;
use crate::session::builder::git_sanitize_branch_name;
use crate::session::WorktreeInfo;

/// Derive the worktree directory leaf for a tied session from its title.
///
/// Reuses the creation-time title slugger (`branch_name_from_title`) so a tied
/// rename produces the same leaf the session would have been created with: an
/// accent-folded, lowercased, dash-collapsed single path component. The title
/// slugger preserves '/' as a git namespace separator, so the result is run
/// through `sanitize_branch_name` to fold slashes to dashes, exactly as
/// `resolve_template` does when deriving a leaf from a branch at creation. It
/// never yields an empty string, a path separator, or `.`/`..` (it falls back
/// to `"session"`), so the result is always a safe sibling-leaf name. Feeding
/// it back through [`edit_worktree_workdir`]'s internal sanitizer is idempotent.
pub fn worktree_leaf_from_title(title: &str) -> String {
    sanitize_branch_name(&crate::session::builder::branch_name_from_title(title))
}

/// The path [`edit_worktree_workdir`] would relocate the worktree to for
/// `new_name`, or `None` when `current_path` has no parent to rename within.
///
/// The single source of truth for the sanitizer chain (git-ref sanitizer, then
/// the path-safe one) so [`worktree_move_required`] cannot drift from the
/// operation it gates.
pub(crate) fn target_worktree_path(current_path: &Path, new_name: &str) -> Option<PathBuf> {
    let parent = current_path.parent()?;
    let new_leaf = sanitize_branch_name(&git_sanitize_branch_name(new_name));
    Some(parent.join(new_leaf))
}

/// The tied-rename destination directory for `title`, as a string: the path
/// [`edit_worktree_workdir`] would move `current_path` to, or `current_path`
/// unchanged when it has no parent to rename within.
///
/// Single source for the destination the duplicate-identity check keys on
/// across the CLI, server, and TUI rename paths: it slugs `title` to a
/// sibling leaf ([`worktree_leaf_from_title`]) and resolves the move target
/// ([`target_worktree_path`]), so the uniqueness gate always tests the exact
/// path the directory would land on rather than the row's current path.
pub(crate) fn derived_worktree_path(current_path: &Path, title: &str) -> String {
    let leaf = worktree_leaf_from_title(title);
    target_worktree_path(current_path, &leaf)
        .unwrap_or_else(|| current_path.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

/// Whether a workdir edit for `new_name` would actually move the worktree
/// directory.
///
/// Callers must gate [`ensure_sandbox_container_released`] on this. That helper
/// removes a stopped sandbox container to free the bind mount, which is only
/// worth doing when the rename is going to `rename(2)` the directory. Three
/// cases reach these endpoints without moving anything, and each one would
/// otherwise destroy a container for no reason:
///
///   - a name that sanitizes to the leaf the worktree already has,
///   - a no-op edit submitting the current name unchanged,
///   - a branch-only edit (`rename_branch` with the title or name unchanged),
///     which [`edit_worktree_workdir`] handles without touching the path.
///
/// The post-move `discard_sandbox_container_after_move` call already gates on
/// `path != current_path` for the same reason; this is the pre-move half of
/// that rule. See #3171 review.
///
/// An empty name is reported as "no move": `edit_worktree_workdir` rejects it
/// with `EmptyName` before doing anything.
pub fn worktree_move_required(current_path: &Path, new_name: &str) -> bool {
    if new_name.trim().is_empty() {
        return false;
    }
    target_worktree_path(current_path, new_name).is_some_and(|p| p != current_path)
}

/// Whether a workdir edit for `new_name` would actually rename the git branch.
///
/// The single source of truth for the branch-change predicate, mirroring
/// [`worktree_move_required`] for the directory-move half. Callers that need to
/// predict "will this edit rename the branch?" (to gate a running-session
/// check, or to decide whether a structured-view worker must be quiesced) route
/// through this so they cannot drift from [`edit_worktree_workdir`]'s own
/// `branch_changes` decision.
///
/// Applies the same git-ref sanitizer (`git_sanitize_branch_name`) the edit
/// applies to `new_name`, so a raw name that sanitizes to the branch the
/// worktree already has is correctly reported as "no rename". A raw leaf
/// comparison would miss that and over-report a rename.
pub fn worktree_branch_rename_required(
    worktree_info: &WorktreeInfo,
    new_name: &str,
    rename_branch: bool,
) -> bool {
    rename_branch && git_sanitize_branch_name(new_name) != worktree_info.branch
}

/// Release a sandbox session's hold on its worktree directory ahead of a
/// `git worktree move`, and report whether the worktree is *still* held.
///
/// `true` means the caller must refuse the move.
///
/// A sandbox container bind-mounts the worktree dir. `git worktree move`
/// `rename(2)`s that dir, and the mount holder makes the rename fail: as
/// `EBUSY` on Linux, and as `EACCES` ("Permission denied") on Docker
/// Desktop for macOS, where the bind is re-exported through the VM's
/// file-sharing layer. Either way git surfaces `fatal: failed to move`.
///
/// Two cases, and the distinction is the whole point of this function:
///
///   - **Running.** The agent is live in there; we can't yank its mount out
///     from under it. Report held and let the caller tell the user to stop
///     the session first.
///   - **Stopped but still present.** `docker stop` does *not* release the
///     bind on Docker Desktop, so the rename fails exactly as it would
///     against a live container, but there is nothing to protect. Discard
///     the container here, which drops the mount, and report not-held. The
///     container is recreated with the new path on next start (see
///     [`discard_sandbox_container_after_move`], which the caller still
///     invokes post-move and which no-ops as `AlreadyGone` when we got here
///     first).
///
/// Before this, the gate tested `probe_running()` alone, so a session the
/// user had just stopped sailed past it and hit the rename failure the gate
/// exists to prevent: trashing a stopped sandboxed session logged
/// `trash worktree relocation skipped: ... Permission denied` and left the
/// worktree in place until a later daemon reconcile happened to remove the
/// container first. See #1927 follow-up, #2596, and #3171.
///
/// `is_sandboxed` is taken so non-sandbox sessions skip the `docker inspect`
/// subprocess entirely.
///
/// Fails closed on a transient `docker inspect` failure: a [`Probe::Unknown`]
/// answer is treated as "possibly running" and blocks the rename, rather
/// than swallowing the failure into a false negative (`unwrap_or(false)`)
/// that would let the rename proceed against a live container. This function
/// is *the* barrier that stops the failed rename, not a best-effort post-move
/// cleanup.
pub fn ensure_sandbox_container_released(session_id: &str, is_sandboxed: bool) -> bool {
    if !is_sandboxed {
        return false;
    }
    let container = DockerContainer::from_session_id(session_id);
    match container.probe_running() {
        Probe::Running => true,
        Probe::NotRunning => {
            // Stopped, but a surviving container still pins the bind mount.
            // Dropping it now is what makes the rename succeed. Non-force, so a
            // container that came back up between the probe above and here is
            // refused rather than force-killed: only the two server callers hold
            // `instance_lock`, and the CLI, TUI, and trash paths race a daemon
            // reconcile or a Start from the dashboard.
            match container.discard_if_stopped() {
                Teardown::Removed => {
                    tracing::info!(
                        target: "containers.runtime",
                        session = %session_id,
                        "removed stopped sandbox container to release its bind mount before the worktree move"
                    );
                    false
                }
                Teardown::AlreadyGone => false,
                // Couldn't drop it, so assume it still holds the mount and
                // fail the rename with a real reason instead of letting git
                // fail with a bare `Permission denied`.
                Teardown::Failed(e) => {
                    tracing::warn!(
                        target: "containers.runtime",
                        session = %session_id,
                        error = %e,
                        "failed to remove stopped sandbox container before the worktree move; reporting the worktree as held"
                    );
                    true
                }
            }
        }
        Probe::Unknown(e) => {
            tracing::warn!(
                target: "containers.runtime",
                session = %session_id,
                error = %e,
                "docker inspect failed while probing sandbox container for the worktree rename gate; failing closed and reporting the worktree as held to prevent a failed rename against a possibly-live container"
            );
            true
        }
    }
}

/// Drop a sandbox session's container after its worktree directory has been
/// moved by a rename.
///
/// A container's bind mounts and working dir are baked in at creation time
/// (`src/containers/runtime_base.rs`); they do NOT follow a host-side
/// `git worktree move`. `get_container_for_instance` reuses an existing
/// stopped container as-is, so without this the restarted container would
/// still mount (and `cd` into) the old path. [`DockerContainer::discard`]
/// forces a fresh `create` with the new path on next start while preserving
/// the session's named ignore volumes (`target/`, `node_modules/`), so the
/// recreated container re-attaches every cache the move did not move; see
/// [`DockerContainer::remove_stranded_named_ignore_volumes`] for the ones it did.
///
/// No-op for non-sandbox sessions, and commonly a no-op
/// ([`Teardown::AlreadyGone`]) on the sandbox path too: the rename gate
/// ([`ensure_sandbox_container_released`]) already discards a stopped
/// container to free the bind mount, so this call is what covers the
/// remaining case of a container that reappeared, plus callers that reach a
/// rename without passing the gate. Best-effort: a failure is logged, not
/// surfaced, since the
/// rename itself has already succeeded. See #1927 follow-up and #2596.
pub fn discard_sandbox_container_after_move(session_id: &str, is_sandboxed: bool) {
    if !is_sandboxed {
        return;
    }
    let container = DockerContainer::from_session_id(session_id);
    match container.discard() {
        Teardown::Removed => tracing::info!(
            target: "containers.runtime",
            session = %session_id,
            "removed stale sandbox container after worktree move; it will be recreated with the new path on next start"
        ),
        Teardown::AlreadyGone => {}
        Teardown::Failed(e) => tracing::warn!(
            target: "containers.runtime",
            session = %session_id,
            "failed to remove stale sandbox container after worktree move: {e}"
        ),
    }
}

/// Stop a sandbox session's container without removing it, so it can be
/// restarted on re-attach.
///
/// No-op for non-sandbox sessions (`is_sandboxed` is taken so they skip the
/// `docker inspect`/`docker stop` subprocesses entirely). A `Probe::Running`
/// container is stopped and a stop failure is propagated; a transient
/// `docker inspect` failure ([`Probe::Unknown`]) still attempts the stop with a
/// `warn!`, so a possibly-live container is not silently abandoned, and a
/// second best-effort `warn!` covers a stop that then also fails.
///
/// Shared by [`Instance::stop`](crate::session::Instance::stop) and the trash
/// path (`trash_session_by_id` via
/// [`prepare_trashed_worktree`](crate::session::trash::prepare_trashed_worktree)):
/// a trashed session is durably stopped, and its container must come down for
/// the same reason `stop` brings it down. The container runs `sleep infinity`
/// for the life of the session and bind-mounts the worktree dir, so leaving it
/// up both leaks a running container for the whole retention window and makes
/// [`relocate_worktree_to_trash`](crate::session::trash::relocate_worktree_to_trash)
/// fail `EBUSY` against the live mount.
///
/// BLOCKING: `docker stop` waits out the container's stop grace period (~10s,
/// because the PID-1 `sleep infinity` ignores SIGTERM until the SIGKILL), so
/// this must never run on the TUI input thread. Every UI-facing caller runs it
/// off-thread: the stop path via [`perform_stop`](crate::session::stop::perform_stop)
/// on the `StopPoller`, the trash path via
/// [`perform_trash`](crate::session::trash::perform_trash) on the `TrashPoller`,
/// and the server via `spawn_blocking`. The CLI runs it inline only because it
/// is a one-shot process with no event loop to starve.
pub fn stop_sandbox_container(session_id: &str, is_sandboxed: bool) -> anyhow::Result<()> {
    if !is_sandboxed {
        return Ok(());
    }
    let container = DockerContainer::from_session_id(session_id);
    match container.probe_running() {
        Probe::Running => container.stop()?,
        Probe::NotRunning => {}
        Probe::Unknown(e) => {
            tracing::warn!(
                target: "containers.runtime",
                session = %session_id,
                error = %e,
                "docker inspect failed while probing sandbox container before stop; attempting stop anyway to avoid leaving a possibly-live container behind"
            );
            if let Err(stop_err) = container.stop() {
                tracing::warn!(
                    target: "containers.runtime",
                    session = %session_id,
                    error = %stop_err,
                    "sandbox container stop failed after probe failure; container may already be gone or docker is unreachable"
                );
            }
        }
    }
    Ok(())
}

/// Inputs for an in-place worktree workdir edit.
pub struct WorktreeEditRequest<'a> {
    /// The session's current worktree metadata.
    pub worktree_info: &'a WorktreeInfo,
    /// The session's current `project_path` (the worktree directory).
    pub current_path: &'a Path,
    /// User-supplied new workdir name (raw; sanitized here).
    pub new_name: &'a str,
    /// Whether to also rename the git branch to match the new name.
    pub rename_branch: bool,
}

/// Result of a successful edit: the values the caller must persist.
#[derive(Debug)]
pub struct WorktreeEditOutcome {
    /// New worktree directory; assign to `Instance.project_path`.
    pub new_path: PathBuf,
    /// `Some(new_branch)` when the branch was renamed; assign to
    /// `worktree_info.branch`. `None` means the branch was left untouched.
    pub new_branch: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum WorktreeEditError {
    #[error("this worktree is not managed by aoe; its workdir name cannot be edited")]
    NotManaged,
    #[error("the new workdir name is empty")]
    EmptyName,
    #[error("the workdir name is unchanged")]
    Unchanged,
    #[error("cannot determine the parent directory of {}", .0.display())]
    NoParent(PathBuf),
    #[error("the current worktree directory {} does not exist", .0.display())]
    SourceMissing(PathBuf),
    #[error("a directory already exists at {}", .0.display())]
    TargetExists(PathBuf),
    #[error("branch '{0}' already exists")]
    BranchExists(String),
    #[error(
        "worktree move failed ({move_err}), and rolling the branch rename back to '{branch}' also failed ({rollback_err}); the repo may be left on the new branch"
    )]
    RollbackFailed {
        move_err: String,
        rollback_err: String,
        branch: String,
    },
    #[error(transparent)]
    Git(#[from] GitError),
}

/// Validate and apply an in-place worktree workdir edit.
///
/// On success the git side effects (optional branch rename, directory move)
/// have already been applied; the returned [`WorktreeEditOutcome`] carries
/// the values the caller must persist to storage and in-memory state. On
/// error nothing is left partially applied: a failed directory move rolls
/// back any branch rename performed in the same call.
pub fn edit_worktree_workdir(
    req: WorktreeEditRequest,
) -> Result<WorktreeEditOutcome, WorktreeEditError> {
    if !req.worktree_info.managed_by_aoe {
        return Err(WorktreeEditError::NotManaged);
    }
    if req.new_name.trim().is_empty() {
        return Err(WorktreeEditError::EmptyName);
    }

    // The new branch name uses the same git-ref sanitizer as creation; the
    // directory leaf uses the path-safe sanitizer (slashes become dashes),
    // mirroring how `resolve_template` derives a leaf from a branch.
    let new_branch = git_sanitize_branch_name(req.new_name);

    let new_path = target_worktree_path(req.current_path, req.new_name)
        .ok_or_else(|| WorktreeEditError::NoParent(req.current_path.to_path_buf()))?;

    let branch_changes =
        worktree_branch_rename_required(req.worktree_info, req.new_name, req.rename_branch);
    let path_changes = new_path != req.current_path;
    if !branch_changes && !path_changes {
        return Err(WorktreeEditError::Unchanged);
    }

    let git = GitWorktree::new(PathBuf::from(&req.worktree_info.main_repo_path))?;

    if !req.current_path.exists() {
        return Err(WorktreeEditError::SourceMissing(
            req.current_path.to_path_buf(),
        ));
    }
    // #2653 fail-closed gate: swallowing `Err` as "absent" would
    // clobber a branch that actually existed or explode inside
    // `rename_branch`. See `GitWorktree::branch_exists` docstring
    // for the tri-state contract.
    if branch_changes && git.branch_exists(&new_branch)? {
        return Err(WorktreeEditError::BranchExists(new_branch));
    }
    if path_changes && new_path.exists() {
        return Err(WorktreeEditError::TargetExists(new_path));
    }

    // Branch first: a ref rename is cheap to undo if the directory move
    // (the riskier step) then fails.
    let mut renamed_branch = false;
    if branch_changes {
        git.rename_branch(&req.worktree_info.branch, &new_branch)?;
        renamed_branch = true;
    }

    if path_changes {
        if let Err(e) = git.move_worktree(req.current_path, &new_path) {
            if renamed_branch {
                if let Err(rollback) = git.rename_branch(&new_branch, &req.worktree_info.branch) {
                    tracing::error!(
                        target: "git.worktree",
                        new = %new_branch,
                        old = %req.worktree_info.branch,
                        "worktree edit: branch-rename rollback failed after move error: {rollback}"
                    );
                    // The repo is now on `new_branch` with the directory still
                    // at its old path. Surface both failures so the caller does
                    // not treat this as a clean "move failed, nothing changed".
                    return Err(WorktreeEditError::RollbackFailed {
                        move_err: e.to_string(),
                        rollback_err: rollback.to_string(),
                        branch: new_branch.clone(),
                    });
                }
            }
            return Err(e.into());
        }
    }

    Ok(WorktreeEditOutcome {
        new_path,
        new_branch: renamed_branch.then_some(new_branch),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn wt_info(branch: &str, main_repo: &str, managed: bool) -> WorktreeInfo {
        WorktreeInfo {
            branch: branch.to_string(),
            main_repo_path: main_repo.to_string(),
            managed_by_aoe: managed,
            created_at: Utc::now(),
            base_branch: None,
        }
    }

    /// The gate that keeps `ensure_sandbox_container_released` (which discards a
    /// stopped container) from firing for a rename that never moves the
    /// directory. Before this, a no-op or branch-only edit destroyed a stopped
    /// sandbox container while leaving the worktree exactly where it was, on all
    /// five gates. Flagged by CodeRabbit on #3171.
    #[test]
    fn worktree_move_required_only_when_the_leaf_changes() {
        let cur = Path::new("/repos/wt/feature-login");

        // A genuinely different name relocates the directory.
        assert!(worktree_move_required(cur, "feature-logout"));

        // The name the worktree already has: no move, so no container discard.
        // This is also the branch-only shape, which reaches these endpoints with
        // the name unchanged and `rename_branch` set; `edit_worktree_workdir`
        // renames the ref without touching the path.
        assert!(!worktree_move_required(cur, "feature-login"));

        // Sanitizes to the current leaf, so still no move. This is the case a
        // raw string comparison against the leaf would miss: the path-safe
        // sanitizer folds '/' to '-', landing back on the leaf already on disk.
        assert!(!worktree_move_required(cur, "feature/login"));

        // Neither sanitizer lowercases (verified against the chain, not
        // assumed), so a case change is a real relocation and must NOT be
        // treated as a no-op.
        assert!(worktree_move_required(cur, "Feature Login"));

        // Empty is rejected upstream with `EmptyName`; nothing moves.
        assert!(!worktree_move_required(cur, ""));
        assert!(!worktree_move_required(cur, "   "));

        // No parent to rename within.
        assert!(!worktree_move_required(Path::new("/"), "anything"));
    }

    /// `worktree_move_required` must agree with `edit_worktree_workdir`'s own
    /// `path_changes` decision, since it exists purely to predict it. Both now
    /// route through `target_worktree_path`, and this pins that they stay
    /// routed through it: a drift here silently re-opens the bug above.
    #[test]
    fn worktree_move_required_agrees_with_the_target_path_it_gates() {
        let cur = Path::new("/repos/wt/feature-login");
        for name in [
            "feature-logout",
            "feature-login",
            "feature/login",
            "Feature Login",
            "wild  name",
        ] {
            let target = target_worktree_path(cur, name).expect("has a parent");
            assert_eq!(
                worktree_move_required(cur, name),
                target != cur,
                "disagreement for {name:?}: target resolved to {}",
                target.display()
            );
        }
    }

    /// `worktree_branch_rename_required` is the branch-half counterpart of
    /// `worktree_move_required`, and must agree with the `branch_changes`
    /// decision `edit_worktree_workdir` actually makes: `rename_branch` armed
    /// AND the git-ref-sanitized name differing from the current branch. The
    /// sanitizer step is the point a raw leaf comparison would get wrong.
    #[test]
    fn worktree_branch_rename_required_matches_the_sanitized_branch_change() {
        let info = wt_info("feature/login", "/tmp/repo", true);
        let cases = [
            // rename_branch off: never a rename, whatever the name.
            ("feature/logout", false, false),
            // A genuinely different name with the toggle on renames the branch.
            ("feature/logout", true, true),
            // Sanitizes back to the current branch: git_sanitize_branch_name is
            // idempotent on an already-valid ref, so this is NOT a rename even
            // with the toggle on. A raw comparison would agree here.
            ("feature/login", true, false),
            // The case a raw string comparison gets WRONG: a forbidden ref
            // char ('~') the git-ref sanitizer folds to '-' and then strips as
            // a trailing dash lands back on the current branch, so the
            // sanitized comparison must report no rename where a raw one would
            // over-report one.
            ("feature/login~", true, false),
        ];
        for (name, rename_branch, expected) in cases {
            assert_eq!(
                worktree_branch_rename_required(&info, name, rename_branch),
                expected,
                "name={name:?} rename_branch={rename_branch}"
            );
        }
    }

    #[test]
    fn holds_worktree_short_circuits_without_sandbox() {
        // The gate that guards `set_worktree_name_for_selected` and
        // `rename_selected` (#2117, #2414): a non-sandbox session must return
        // false before touching the container runtime, so a plain worktree
        // rename never pays a `docker inspect`. The live-container branch is the
        // same helper the tied-rename path already relies on.
        assert!(!ensure_sandbox_container_released("any-session-id", false));
    }

    #[test]
    fn discard_after_move_short_circuits_without_sandbox() {
        // After the #2596 fix, the `is_sandboxed` guard is the ONLY thing that
        // keeps a plain (non-sandbox) worktree rename from spawning a `docker`
        // subprocess. Time-bound to catch a future edit that reorders the
        // guard below the runtime call: the sandbox-off path returns before
        // any `DockerContainer::from_session_id` allocation, so wall time is
        // sub-millisecond; 100 ms is a CI-safe upper bound that still catches
        // a real `docker inspect` (dozens to hundreds of ms) or a
        // `container.discard()` shell-out. The Teardown-variant branches
        // (Removed / AlreadyGone / Failed) are covered by the
        // `classify_removal` unit tests in `containers::mod`.
        let start = std::time::Instant::now();
        discard_sandbox_container_after_move("any-session-id", false);
        let elapsed = start.elapsed();
        assert!(
            elapsed < std::time::Duration::from_millis(100),
            "non-sandbox path must short-circuit before any container runtime call; elapsed = {elapsed:?}"
        );
    }

    #[test]
    fn leaf_from_title_slugifies() {
        assert_eq!(worktree_leaf_from_title("Auth refactor"), "auth-refactor");
        assert_eq!(
            worktree_leaf_from_title("Fix: the/thing (v2)"),
            "fix-the-thing-v2"
        );
        // A slash-bearing title yields a slashed branch but the folder leaf
        // must stay a single, flat path component.
        let leaf = worktree_leaf_from_title("jacob/feature-1");
        assert_eq!(leaf, "jacob-feature-1");
        assert!(!leaf.contains('/'));
    }

    #[test]
    fn leaf_from_title_never_empty_or_traversal() {
        // Punctuation-only and dot titles fall back / collapse rather than
        // producing an empty leaf or a "."/".." path component.
        assert_eq!(worktree_leaf_from_title("..."), "session");
        assert_eq!(worktree_leaf_from_title("   "), "session");
        let leaf = worktree_leaf_from_title("../escape");
        assert!(!leaf.contains('/') && leaf != ".." && !leaf.is_empty());
    }

    #[test]
    fn rejects_unmanaged_worktree() {
        let info = wt_info("old", "/tmp/repo", false);
        let err = edit_worktree_workdir(WorktreeEditRequest {
            worktree_info: &info,
            current_path: Path::new("/tmp/wt/old"),
            new_name: "new",
            rename_branch: false,
        })
        .unwrap_err();
        assert!(matches!(err, WorktreeEditError::NotManaged));
    }

    #[test]
    fn rejects_empty_name() {
        let info = wt_info("old", "/tmp/repo", true);
        let err = edit_worktree_workdir(WorktreeEditRequest {
            worktree_info: &info,
            current_path: Path::new("/tmp/wt/old"),
            new_name: "   ",
            rename_branch: false,
        })
        .unwrap_err();
        assert!(matches!(err, WorktreeEditError::EmptyName));
    }

    #[test]
    fn rejects_unchanged_name_without_branch_rename() {
        // Leaf derived from "old" is "old", so the path does not change and
        // branch rename is off: nothing would happen.
        let info = wt_info("old", "/tmp/repo", true);
        let err = edit_worktree_workdir(WorktreeEditRequest {
            worktree_info: &info,
            current_path: Path::new("/tmp/wt/old"),
            new_name: "old",
            rename_branch: false,
        })
        .unwrap_err();
        assert!(matches!(err, WorktreeEditError::Unchanged));
    }
}
