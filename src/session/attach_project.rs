//! Attach a repo to a session that already exists (#3103).
//!
//! Creation-time multi-repo lives in [`super::builder::create_workspace`],
//! which lays every repo out under one aoe-created workspace directory. That
//! shape is only reachable at creation, so realizing mid-task that you also
//! need the frontend repo used to mean destroying the session. This module is
//! the post-creation counterpart: it creates one worktree, records it in
//! [`Instance::attached_repos`], and leaves the session's `project_path` and
//! `cwd` untouched. Widening the agent's view is the caller's job, via
//! [`Instance::additional_root_paths`] and the ACP `additional_directories`
//! field.
//!
//! Two invariants shape the code below.
//!
//! **Nothing lands next to the user's own checkout.** For a session created
//! with `--worktree` or in place, the attached worktree goes under an
//! aoe-owned per-session directory keyed by the full session id, so two
//! sessions attaching the same repo on the same branch cannot collide and
//! cleanup is unambiguous. Only a session that already has a workspace keeps
//! its repos together, under the existing `workspace_dir`.
//!
//! **A branch aoe did not create is never touched.** The session's branch name
//! is a suggestion. If the added repo already has that branch, the attach
//! refuses unless the caller explicitly opts into reusing it, and the reuse is
//! recorded so session deletion leaves the branch alone.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::Utc;

use super::builder;
use super::instance::AttachedRepo;
use super::storage::Storage;
use crate::git::GitWorktree;

/// Directory under the app data dir holding worktrees for repos attached to
/// non-workspace sessions. One subdirectory per session id.
const ATTACHED_DIR: &str = "attached-repos";

/// What to do when the resolved branch already exists in the repo being
/// attached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExistingBranch {
    /// Refuse. The default: the branch may carry unrelated commits, and
    /// checking it out would silently feed the agent the wrong tree.
    Refuse,
    /// Check the existing branch out. Recorded as not aoe-created, so session
    /// deletion leaves it in place.
    Attach,
}

/// Where an attached worktree should live, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Placement {
    /// The session already groups its repos under one aoe-created workspace
    /// directory; keep the new repo with its siblings. Also means the worktree
    /// lands inside the session `cwd`, so it works even against an agent that
    /// does not support additional directories.
    Workspace(PathBuf),
    /// Everything else: an aoe-owned per-session directory.
    SessionOwned(PathBuf),
}

impl Placement {
    fn path(&self) -> &Path {
        match self {
            Placement::Workspace(p) | Placement::SessionOwned(p) => p,
        }
    }
}

/// Outcome of a successful attach, for the caller to report.
#[derive(Debug, Clone)]
pub struct AttachOutcome {
    pub repo: AttachedRepo,
    /// Non-fatal warnings from worktree creation (submodule init, fetch).
    pub warnings: Vec<String>,
    /// Whether the worktree landed inside the session's `cwd`, in which case
    /// the agent can already see it without additional-directory support.
    pub inside_cwd: bool,
}

/// Resolve the directory an attached worktree goes in.
///
/// `workspace_dir` is the session's existing workspace directory when it has
/// one. `app_dir` is the aoe data dir. Pure so the layout rules are testable
/// without touching git or the filesystem.
fn resolve_placement(
    workspace_dir: Option<&str>,
    app_dir: &Path,
    session_id: &str,
    repo_name: &str,
) -> Placement {
    match workspace_dir {
        Some(dir) => Placement::Workspace(PathBuf::from(dir).join(repo_name)),
        None => {
            Placement::SessionOwned(app_dir.join(ATTACHED_DIR).join(session_id).join(repo_name))
        }
    }
}

/// The directory leaf an attached repo is known by.
///
/// Taken from the main repo rather than the path the user typed, so pointing at
/// a worktree of a repo yields the repo's own name and collides with an
/// existing entry for it instead of sneaking in under a second name.
fn repo_leaf_name(main_repo_path: &Path) -> String {
    main_repo_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "repo".to_string())
}

/// Reject an attach that duplicates a repo the session already has.
///
/// Identity is the resolved main repo path, so a symlinked path, a bare path,
/// and one of the repo's own worktrees all resolve to the same repo and are
/// caught. The leaf name is checked separately because it is the directory
/// name and the label used for repo-relative path rendering, so two different
/// repos with the same leaf would be indistinguishable.
fn reject_duplicate(
    instance: &super::Instance,
    main_repo_path: &Path,
    repo_name: &str,
) -> Result<()> {
    let incoming = canonical(main_repo_path);

    if canonical(Path::new(&instance.project_path)) == incoming {
        bail!(
            "'{}' is already this session's own repo",
            main_repo_path.display()
        );
    }
    if let Some(wt) = &instance.worktree_info {
        if canonical(Path::new(&wt.main_repo_path)) == incoming {
            bail!(
                "'{}' is already this session's own repo",
                main_repo_path.display()
            );
        }
    }

    for repo in instance.all_repos() {
        if canonical(Path::new(&repo.main_repo_path)) == incoming {
            bail!(
                "'{}' is already attached to this session as '{}'",
                main_repo_path.display(),
                repo.name
            );
        }
        // Case-insensitive because the worktree directory leaf collides on
        // macOS and Windows filesystems even when the names differ in case.
        if repo.name.eq_ignore_ascii_case(repo_name) {
            bail!(
                "this session already has a repo directory named '{}' (from '{}'); \
                 attaching another would collide on disk",
                repo.name,
                repo.main_repo_path
            );
        }
    }
    Ok(())
}

/// Best-effort canonicalization for identity comparison. Falls back to the
/// path as given when it does not exist, which still compares correctly
/// against another non-existent path spelled the same way.
fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// The branch a session's worktrees are on, if it has one.
///
/// A workspace session records it on `workspace_info`, a single-repo worktree
/// session on `workspace_info`. A plain in-place session has neither: aoe never
/// created a branch for it, so there is no session branch to mirror and
/// [`branch_for_plain_session`] supplies one instead.
fn session_branch(instance: &super::Instance) -> Option<&str> {
    instance
        .worktree_info
        .as_ref()
        .map(|w| w.branch.as_str())
        .or_else(|| instance.workspace_info.as_ref().map(|w| w.branch.as_str()))
}

/// The branch to create in a repo attached to a session that has none of its
/// own (a plain in-place session).
///
/// Not the added repo's default branch: that branch is checked out in the repo
/// itself, so `git worktree add` would refuse it, which would make attaching to
/// an in-place session impossible. Derived from the session title through the
/// same slugger creation uses, so the branch reads like one aoe would have made
/// for a worktree session with that title.
fn branch_for_plain_session(title: &str) -> String {
    let slug = builder::git_sanitize_branch_name(&builder::branch_name_from_title(title));
    if slug.is_empty() {
        "aoe-attached".to_string()
    } else {
        slug
    }
}

/// The branch to check out in the repo being attached, and whether aoe has to
/// create it.
struct BranchPlan {
    branch: String,
    create: bool,
    base: Option<String>,
}

/// Decide the branch for the attached repo.
///
/// The session branch is only a suggestion: branch names are repo-local, so a
/// matching name in another repo does not imply matching meaning. When the name
/// is absent from the added repo it is created from that repo's own base; when
/// it is present the caller has to say explicitly that reusing it is intended.
fn plan_branch(
    git_wt: &GitWorktree,
    suggested: &str,
    base: Option<String>,
    on_existing: ExistingBranch,
) -> Result<BranchPlan> {
    let branch = builder::git_sanitize_branch_name(suggested);
    // Checked immediately before `create_worktree` runs, so a branch created
    // between here and there surfaces as a git error rather than being
    // silently reused.
    if git_wt
        .branch_exists(&branch)
        .with_context(|| format!("could not check whether branch '{branch}' exists"))?
    {
        if on_existing == ExistingBranch::Refuse {
            bail!(
                "branch '{branch}' already exists in the repo being attached and may hold \
                 unrelated commits. Re-run with --attach-existing-branch to check it out as-is, \
                 and note that aoe will then leave it alone when the session is deleted."
            );
        }
        return Ok(BranchPlan {
            branch,
            create: false,
            base: None,
        });
    }

    Ok(BranchPlan {
        branch,
        create: true,
        base,
    })
}

/// Refuse when the resolved branch is already checked out in another worktree
/// of the added repo. `git worktree add` would fail anyway; catching it here
/// gives the user the path that is holding it.
fn reject_branch_checked_out(git_wt: &GitWorktree, branch: &str) -> Result<()> {
    let worktrees = git_wt
        .list_worktrees()
        .context("could not list the added repo's worktrees")?;
    if let Some(existing) = worktrees
        .iter()
        .find(|w| w.branch.as_deref() == Some(branch))
    {
        bail!(
            "branch '{}' is already checked out at {} in the repo being attached",
            branch,
            existing.path.display()
        );
    }
    Ok(())
}

/// Validate the request and create the worktree, without persisting anything.
///
/// Split from [`attach`] because the two callers persist differently: the CLI
/// and the daemon write through [`Storage::update`], while the TUI mutates its
/// in-memory instance map and saves. Both need the same validation and the same
/// filesystem work, and both need [`PreparedAttach::rollback`] if their own
/// persist fails.
pub fn prepare(
    instance: &super::Instance,
    profile: &str,
    repo_path: &Path,
    on_existing: ExistingBranch,
) -> Result<PreparedAttach> {
    if !GitWorktree::is_git_repo(repo_path) {
        bail!(
            "not a git repository: {}\nAttaching a project needs a git repo so aoe can create a \
             worktree for it.",
            repo_path.display()
        );
    }

    let main_repo_path = GitWorktree::find_main_repo(repo_path)?;
    let main_repo_path = canonical(&main_repo_path);
    let repo_name = repo_leaf_name(&main_repo_path);
    reject_duplicate(instance, &main_repo_path, &repo_name)?;

    // Resolved against the repo being attached: it is the repo a worktree gets
    // created in, so its own `.agent-of-empires/config.toml` governs submodule
    // init and the default base branch.
    let config = super::repo_config::resolve_config_with_repo_or_warn(profile, &main_repo_path);
    let git_wt = GitWorktree::new(main_repo_path.clone())?
        .with_init_submodules(config.worktree.init_submodules);

    let base = builder::resolve_base_branch(
        None,
        builder::project_base_branches(profile)
            .get(&super::projects::canonical_key(
                &main_repo_path.to_string_lossy(),
            ))
            .map(String::as_str),
        config.worktree.default_base_branch.as_deref(),
    );
    // The session's own branch when it has one, else one derived from its title.
    let suggested = session_branch(instance)
        .map(str::to_string)
        .unwrap_or_else(|| branch_for_plain_session(&instance.title));
    let plan = plan_branch(&git_wt, &suggested, base, on_existing)?;
    reject_branch_checked_out(&git_wt, &plan.branch)?;

    let placement = resolve_placement(
        instance
            .workspace_info
            .as_ref()
            .map(|ws| ws.workspace_dir.as_str()),
        &super::get_app_dir()?,
        &instance.id,
        &repo_name,
    );
    let worktree_path = placement.path().to_path_buf();
    if worktree_path.exists() {
        bail!(
            "{} already exists; remove it or detach the repo that owns it before attaching",
            worktree_path.display()
        );
    }
    if let Some(parent) = worktree_path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "could not create the attachment directory {}",
                parent.display()
            )
        })?;
    }

    let warnings = git_wt
        .create_worktree(
            &plan.branch,
            &worktree_path,
            plan.create,
            plan.base.as_deref(),
        )
        .with_context(|| format!("could not create a worktree for '{repo_name}'"))?;

    let repo = AttachedRepo {
        name: repo_name,
        source_path: main_repo_path.to_string_lossy().to_string(),
        branch: plan.branch.clone(),
        worktree_path: worktree_path.to_string_lossy().to_string(),
        main_repo_path: main_repo_path.to_string_lossy().to_string(),
        worktree_managed_by_aoe: true,
        // False when the branch was already there and the caller opted into
        // reusing it, so deleting the session leaves the user's branch alone.
        branch_created_by_aoe: plan.create,
        attached_at: Utc::now(),
    };

    Ok(PreparedAttach {
        outcome: AttachOutcome {
            repo,
            warnings,
            inside_cwd: matches!(placement, Placement::Workspace(_)),
        },
        main_repo_path,
        created_branch: plan.create.then_some(plan.branch),
    })
}

/// A created worktree that has not been recorded on the session yet.
///
/// Holds what [`Self::rollback`] needs, so a caller whose persist fails can
/// undo the filesystem work and leave no orphan behind.
pub struct PreparedAttach {
    pub outcome: AttachOutcome,
    main_repo_path: PathBuf,
    /// Set only when this attempt created the branch, so a rollback never
    /// deletes a branch the user already had.
    created_branch: Option<String>,
}

impl PreparedAttach {
    /// Undo the worktree (and the branch, when this attempt created it).
    ///
    /// Best effort: the caller's persist failure is the error worth reporting,
    /// and a leftover worktree is recoverable with `aoe worktree cleanup`.
    pub fn rollback(&self) {
        let Ok(git_wt) = GitWorktree::new(self.main_repo_path.clone()) else {
            return;
        };
        let worktree = PathBuf::from(&self.outcome.repo.worktree_path);
        let _ = git_wt.remove_worktree(&worktree, true);
        if let Some(branch) = &self.created_branch {
            let _ = git_wt.delete_branch(branch);
        }
    }
}

/// Attach `repo_path` to the session identified by `session_id`.
///
/// Creates the worktree first, then persists. A persist failure rolls the
/// worktree back so a failed attach leaves nothing behind.
pub fn attach(
    storage: &Storage,
    profile: &str,
    session_id: &str,
    repo_path: &Path,
    on_existing: ExistingBranch,
) -> Result<AttachOutcome> {
    let instances = storage.load()?;
    let instance = instances
        .iter()
        .find(|i| i.id == session_id)
        .with_context(|| format!("session not found: {session_id}"))?;

    let prepared = prepare(instance, profile, repo_path, on_existing)?;

    let id = session_id.to_string();
    let to_store = prepared.outcome.repo.clone();
    let persisted = storage.update(|instances, _groups| {
        let inst = instances
            .iter_mut()
            .find(|i| i.id == id)
            .with_context(|| format!("session not found: {id}"))?;
        inst.attached_repos.push(to_store);
        Ok(())
    });

    if let Err(e) = persisted {
        prepared.rollback();
        return Err(e).with_context(|| {
            format!(
                "could not record the attached repo; removed the worktree at {}",
                prepared.outcome.repo.worktree_path
            )
        });
    }

    Ok(prepared.outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{Instance, WorkspaceInfo, WorkspaceRepo, WorktreeInfo};

    fn workspace_instance() -> Instance {
        let mut inst = Instance::new("WS", "/tmp/ws");
        inst.workspace_info = Some(WorkspaceInfo {
            branch: "feature/abc".to_string(),
            workspace_dir: "/tmp/ws".to_string(),
            repos: vec![WorkspaceRepo {
                name: "backend".to_string(),
                source_path: "/tmp/src/backend".to_string(),
                branch: "feature/abc".to_string(),
                worktree_path: "/tmp/ws/backend".to_string(),
                main_repo_path: "/tmp/src/backend".to_string(),
                managed_by_aoe: true,
            }],
            created_at: Utc::now(),
            cleanup_on_delete: true,
        });
        inst
    }

    /// A workspace session keeps its repos together; everything else gets an
    /// aoe-owned directory keyed by session id, never a path beside the user's
    /// checkout.
    #[test]
    fn placement_keeps_workspace_repos_together() {
        let app = PathBuf::from("/home/u/.agent-of-empires");
        assert_eq!(
            resolve_placement(Some("/tmp/ws"), &app, "sess-1", "frontend"),
            Placement::Workspace(PathBuf::from("/tmp/ws/frontend"))
        );
        assert_eq!(
            resolve_placement(None, &app, "sess-1", "frontend"),
            Placement::SessionOwned(PathBuf::from(
                "/home/u/.agent-of-empires/attached-repos/sess-1/frontend"
            ))
        );
    }

    /// The session id in the path is what keeps two sessions attaching the same
    /// repo on the same branch from fighting over one worktree, which is the
    /// failure the repo's own branch-only `worktree.path_template` would have.
    #[test]
    fn placement_is_unique_per_session() {
        let app = PathBuf::from("/app");
        let a = resolve_placement(None, &app, "sess-a", "frontend");
        let b = resolve_placement(None, &app, "sess-b", "frontend");
        assert_ne!(a, b);
    }

    #[test]
    fn repo_leaf_name_uses_the_main_repo_directory() {
        assert_eq!(repo_leaf_name(Path::new("/tmp/src/frontend")), "frontend");
        assert_eq!(repo_leaf_name(Path::new("/")), "repo");
    }

    #[test]
    fn duplicate_by_main_repo_path_is_rejected() {
        let inst = workspace_instance();
        let err = reject_duplicate(&inst, Path::new("/tmp/src/backend"), "backend-alias")
            .expect_err("the same repo must not attach twice");
        assert!(
            err.to_string().contains("already attached"),
            "unexpected error: {err}"
        );
    }

    /// A different repo that happens to share a directory leaf would land on
    /// the same worktree path and render identically in repo-relative output.
    #[test]
    fn duplicate_by_leaf_name_is_rejected_case_insensitively() {
        let inst = workspace_instance();
        let err = reject_duplicate(&inst, Path::new("/other/src/BackEnd"), "BackEnd")
            .expect_err("a colliding directory leaf must not attach");
        assert!(
            err.to_string().contains("collide on disk"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn attaching_the_sessions_own_repo_is_rejected() {
        let mut inst = Instance::new("WT", "/tmp/worktrees/feature");
        inst.worktree_info = Some(WorktreeInfo {
            branch: "feature/abc".to_string(),
            main_repo_path: "/tmp/src/backend".to_string(),
            managed_by_aoe: true,
            created_at: Utc::now(),
            base_branch: None,
        });
        let err = reject_duplicate(&inst, Path::new("/tmp/src/backend"), "backend")
            .expect_err("the session's own repo must not attach to itself");
        assert!(
            err.to_string().contains("already this session's own repo"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn a_genuinely_new_repo_is_accepted() {
        let inst = workspace_instance();
        reject_duplicate(&inst, Path::new("/tmp/src/frontend"), "frontend").unwrap();
    }

    /// A plain in-place session gets a branch derived from its title, never the
    /// added repo's default branch: that one is checked out in the repo itself,
    /// so `git worktree add` would refuse it and attaching to an in-place
    /// session could never succeed.
    #[test]
    fn plain_session_branch_comes_from_the_title() {
        assert_eq!(
            branch_for_plain_session("Fix the auth bug"),
            "fix-the-auth-bug"
        );
        // Never empty, so the branch name is always valid.
        assert!(!branch_for_plain_session("").is_empty());
        assert!(!branch_for_plain_session("///").is_empty());
    }

    #[test]
    fn session_branch_prefers_worktree_then_workspace() {
        assert_eq!(session_branch(&workspace_instance()), Some("feature/abc"));

        let mut wt = Instance::new("WT", "/tmp/wt");
        wt.worktree_info = Some(WorktreeInfo {
            branch: "fix/xyz".to_string(),
            main_repo_path: "/tmp/src/backend".to_string(),
            managed_by_aoe: true,
            created_at: Utc::now(),
            base_branch: None,
        });
        assert_eq!(session_branch(&wt), Some("fix/xyz"));

        // A plain in-place session has no aoe-created branch to mirror.
        assert_eq!(session_branch(&Instance::new("Plain", "/tmp/plain")), None);
    }
}
