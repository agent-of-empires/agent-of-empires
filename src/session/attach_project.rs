//! Attach a repo to a session that already exists, by converting it into a
//! multi-repo workspace (#3103).
//!
//! Creation-time multi-repo lives in [`super::builder::create_workspace`], which
//! lays every repo out under one aoe-created workspace directory. That shape used
//! to be reachable only at creation, so realizing mid-task that you also need the
//! frontend repo meant destroying the session. This module is the post-creation
//! counterpart, and it produces the *same* shape: after attaching, a session is
//! indistinguishable from one created multi-repo, with both repos side by side in
//! one workspace directory and `workspace_info.repos` listing them.
//!
//! Converting rather than keeping a second list of "attached" repos is the whole
//! design. The alternative, leaving the session where it is and pointing the agent
//! at a worktree parked somewhere else, needs the ACP `additional_directories`
//! capability, a sandbox path-map entry per extra root, and a degradation path for
//! agents that do not advertise it, and it still leaves a session that only half
//! looks multi-repo. Landing both repos under the session `cwd` needs none of that.
//!
//! Three starting shapes, one result:
//!
//! - **Already a workspace**: the new repo's worktree is created inside the
//!   existing `workspace_dir`. Nothing moves.
//! - **Worktree session**: a new workspace directory is created and the session's
//!   existing worktree is moved into it with `git worktree move`, so uncommitted
//!   work travels with it.
//! - **In-place session**: a new workspace directory gets a *fresh* worktree of
//!   the session's repo. The user's own checkout is never moved or deleted, which
//!   is why this case refuses when that checkout is dirty: the session's cwd moves
//!   to the new worktree and uncommitted work would be left behind.
//!
//! Two invariants shape the code below.
//!
//! **`workspace_dir` only ever contains worktrees aoe created.** That is why the
//! in-place case creates a worktree instead of adopting the user's checkout.
//! `deletion` still verifies the layout with `workspace_dir_is_aoe_owned` rather
//! than trusting the record, and its final removal is non-recursive, so anything
//! else still sitting under the directory keeps it on disk instead of being
//! deleted with it.
//!
//! **A branch aoe did not create is never touched.** The session's branch name is a
//! suggestion. If the added repo already has that branch, the attach refuses unless
//! the caller explicitly opts into reusing it, and the reuse is recorded on the
//! repo (`branch_preexisting`) so session deletion leaves the branch alone.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::Utc;

use super::builder;
use super::instance::{WorkspaceInfo, WorkspaceRepo};
use super::storage::Storage;
use crate::git::GitWorktree;

/// What to do when the resolved branch already exists in the repo being
/// attached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExistingBranch {
    /// Refuse. The default: the branch may carry unrelated commits, and
    /// checking it out would silently feed the agent the wrong tree.
    Refuse,
    /// Check the existing branch out. Recorded as pre-existing, so session
    /// deletion leaves it in place.
    Attach,
}

/// Outcome of a successful attach, for the caller to report.
#[derive(Debug, Clone)]
pub struct AttachOutcome {
    /// The repo that was added.
    pub repo: WorkspaceRepo,
    /// Non-fatal warnings from worktree creation (submodule init, fetch).
    pub warnings: Vec<String>,
    /// Set when the session was converted into a workspace, carrying its new
    /// `project_path`. `None` when it was already a workspace and nothing moved.
    ///
    /// The surfaces report this: the session's working directory changing is the
    /// one user-visible consequence of the conversion, and anything the user had
    /// open at the old path needs to know.
    pub moved_to: Option<String>,
    /// The workspace the session now has, as persisted.
    ///
    /// Carried on the outcome so the daemon can mirror the change into its
    /// in-memory instance without re-reading from disk; the respawn immediately
    /// after reads that copy to build the container mount set and the agent cwd.
    pub workspace_info: WorkspaceInfo,
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
/// A single-repo worktree session records it on `worktree_info`, which is
/// checked first; a multi-repo workspace session records it on
/// `workspace_info`. A plain in-place session has neither: aoe never
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

/// What has to happen to the session's own repo before the new one can sit
/// beside it.
enum Conversion {
    /// Already a workspace: only the new repo's worktree is created, inside the
    /// existing `workspace_dir`. Nothing moves and `project_path` is unchanged.
    Append { workspace_dir: PathBuf },
    /// Worktree session: create a workspace directory and `git worktree move` the
    /// session's existing worktree into it. A move, so uncommitted work travels.
    MoveIn {
        workspace_dir: PathBuf,
        primary: WorkspaceRepo,
        from: PathBuf,
    },
    /// In-place session: create a workspace directory and a fresh worktree of the
    /// session's repo in it. The user's own checkout is left exactly as it is.
    WorktreePrimary {
        workspace_dir: PathBuf,
        primary: WorkspaceRepo,
        create_branch: bool,
        base: Option<String>,
    },
}

impl Conversion {
    fn workspace_dir(&self) -> &Path {
        match self {
            Conversion::Append { workspace_dir }
            | Conversion::MoveIn { workspace_dir, .. }
            | Conversion::WorktreePrimary { workspace_dir, .. } => workspace_dir,
        }
    }
}

/// Decide how to make room for the new repo, and where the workspace lands.
///
/// The workspace directory comes from the same `workspace_path_template` and
/// `compute_path` creation uses, seeded with the session id, so a converted
/// session sits exactly where an equivalent created-multi-repo session would.
fn plan_conversion(
    instance: &super::Instance,
    profile: &str,
    on_existing: ExistingBranch,
) -> Result<Conversion> {
    if let Some(ws) = &instance.workspace_info {
        return Ok(Conversion::Append {
            workspace_dir: PathBuf::from(&ws.workspace_dir),
        });
    }

    // The session's own repo, whichever shape it is in. For a worktree session
    // that is `worktree_info.main_repo_path`; for an in-place session the
    // checkout at `project_path` is itself in the repo.
    let current = PathBuf::from(&instance.project_path);
    let main_repo = match &instance.worktree_info {
        Some(wt) => canonical(Path::new(&wt.main_repo_path)),
        None => canonical(&GitWorktree::find_main_repo(&current)?),
    };
    let primary_name = repo_leaf_name(&main_repo);
    let git_wt = GitWorktree::new(main_repo.clone())?;
    let config = super::repo_config::resolve_config_with_repo_or_warn(profile, &main_repo);
    let workspace_dir = git_wt.compute_path(
        session_branch(instance).unwrap_or(&branch_for_plain_session(&instance.title)),
        &config.worktree.workspace_path_template,
        &instance.id[..8.min(instance.id.len())],
    )?;
    if workspace_dir.exists() {
        bail!(
            "{} already exists; remove it before attaching a project to this session",
            workspace_dir.display()
        );
    }
    let primary_worktree = workspace_dir.join(&primary_name);

    // Only an aoe-created worktree is ours to relocate. A session pointed at a
    // worktree the user made themselves falls through to the in-place path below,
    // so nothing of theirs moves.
    if let Some(wt) = instance.worktree_info.as_ref().filter(|w| w.managed_by_aoe) {
        return Ok(Conversion::MoveIn {
            workspace_dir,
            primary: WorkspaceRepo {
                name: primary_name,
                source_path: main_repo.to_string_lossy().to_string(),
                branch: wt.branch.clone(),
                worktree_path: primary_worktree.to_string_lossy().to_string(),
                main_repo_path: main_repo.to_string_lossy().to_string(),
                managed_by_aoe: true,
                // `false` means aoe owns the branch, so delete removes it.
                // `WorktreeInfo` records no branch authorship, so a worktree aoe
                // made that was pointed at a pre-existing branch is
                // indistinguishable here. This is parity with main, where any
                // `managed_by_aoe` worktree's branch is deleted; it is not the
                // conservative choice, and closing it needs an authorship field
                // on `WorktreeInfo`.
                branch_preexisting: false,
            },
            from: current,
        });
    }

    // In-place, or a worktree the user created. The session's cwd is about to
    // become a different directory on a different branch, so uncommitted work in
    // the current checkout would silently stop being part of the session.
    if let Some(msg) = crate::git::cleanup::dirty_worktree_message(&current) {
        bail!(
            "{} has uncommitted changes, and attaching a project moves this session into a new \
             workspace directory that would leave them behind.\n\nCommit or stash them first, \
             then attach.\n\n{}",
            current.display(),
            msg
        );
    }

    let base = builder::resolve_base_branch(
        None,
        builder::project_base_branches(profile)
            .get(&super::projects::canonical_key(
                &main_repo.to_string_lossy(),
            ))
            .map(String::as_str),
        config.worktree.default_base_branch.as_deref(),
    );
    // A fresh worktree cannot check out the branch the user's checkout already
    // has, so the session gets one derived from its title, planned against the
    // same opt-in rule the added repo uses.
    let plan = plan_branch(
        &git_wt,
        &branch_for_plain_session(&instance.title),
        base,
        on_existing,
    )?;
    reject_branch_checked_out(&git_wt, &plan.branch)?;

    Ok(Conversion::WorktreePrimary {
        workspace_dir,
        primary: WorkspaceRepo {
            name: primary_name,
            source_path: main_repo.to_string_lossy().to_string(),
            branch: plan.branch,
            worktree_path: primary_worktree.to_string_lossy().to_string(),
            main_repo_path: main_repo.to_string_lossy().to_string(),
            managed_by_aoe: true,
            branch_preexisting: !plan.create,
        },
        create_branch: plan.create,
        base: plan.base,
    })
}

/// Validate the request and create the worktree, without persisting anything.
///
/// Split from [`attach`] because the two callers persist differently: the CLI
/// and the daemon write through [`Storage::update`], while the TUI mutates its
/// in-memory instance map and saves. Both need the same validation and the same
/// filesystem work, and both need [`PreparedAttach::rollback`] if their own
/// persist fails.
pub fn plan(
    instance: &super::Instance,
    profile: &str,
    repo_path: &Path,
    on_existing: ExistingBranch,
) -> Result<AttachPlan> {
    // A scratch session has no repo of its own: its cwd is `<app_dir>/scratch/
    // <id>/`, which deletion removes wholesale. Attaching would give it a repo
    // its own workflow has no place for, and the result reads as a multi-repo
    // session that is really a scratchpad. The choke point every surface shares,
    // so the CLI and the REST endpoint refuse it the same way the pickers do.
    if instance.scratch {
        bail!(
            "'{}' is a scratch session, which has no repo to attach to. Create a session on the \
             repo instead.",
            instance.title
        );
    }

    // Lifecycle states that are never a legitimate moment to attach, on any
    // surface. `Deleting` is the dangerous one: the deletion pass has already
    // read the session's repo list, so a worktree created in that window is
    // orphaned, its record about to be dropped with the session. A trashed or archived
    // session has its agent deliberately stopped, so the worktree would be
    // created for nothing.
    //
    // `Running` / `Waiting` / `Starting` are deliberately NOT here. The daemon
    // refuses those on the authoritative in-flight-turn probe, which lets it
    // accept a Running session that is merely idle between turns; gating on the
    // status here would make it strictly coarser. Surfaces without a handle on
    // the event store apply `Status::blocks_worktree_edit()` themselves.
    if matches!(
        instance.status,
        super::Status::Creating | super::Status::Deleting
    ) {
        bail!(
            "'{}' is still being created or is being deleted; wait for it to settle before \
             attaching a project.",
            instance.title
        );
    }
    if instance.is_trashed() {
        bail!(
            "'{}' is in the trash; restore it before attaching a project.",
            instance.title
        );
    }
    if instance.is_archived() {
        bail!(
            "'{}' is archived and its agent stays stopped; unarchive it before attaching a \
             project.",
            instance.title
        );
    }
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

    // Plan the conversion before touching anything, so a refusal (dirty
    // checkout, workspace path taken, branch already checked out) happens with
    // nothing created.
    let conversion = plan_conversion(instance, profile, on_existing)?;

    let workspace_dir = conversion.workspace_dir().to_path_buf();
    let worktree_path = workspace_dir.join(&repo_name);
    if worktree_path.exists() {
        bail!(
            "{} already exists; remove it, or delete the session that owns it, before attaching",
            worktree_path.display()
        );
    }

    Ok(AttachPlan {
        // Appending to an existing workspace leaves `project_path` alone; the
        // other two shapes move the session into a new workspace directory.
        moves_session: !matches!(conversion, Conversion::Append { .. }),
        conversion,
        workspace_dir,
        added_name: repo_name,
        added_main_repo: main_repo_path,
        added_branch: plan,
        added_worktree: worktree_path,
        init_submodules: config.worktree.init_submodules,
    })
}

/// A validated attach, with nothing written yet.
///
/// Split from [`execute`] so a caller can find out that an attach will be
/// refused *before* quiescing the session for it. The moving shapes need the
/// worker stopped and the sandbox container removed first (a container holds the
/// worktree as an active mount, so `rename(2)` on it fails `EBUSY`), and doing
/// that for an attach that then fails validation would stop a session for
/// nothing.
pub struct AttachPlan {
    /// True when the session's working directory changes, so the caller has to
    /// stop the session around [`execute`] and start it again afterwards.
    ///
    /// False only when the session is already a workspace and the new repo just
    /// appears inside it, which is the one shape that needs no quiescing.
    pub moves_session: bool,
    conversion: Conversion,
    workspace_dir: PathBuf,
    added_name: String,
    added_main_repo: PathBuf,
    added_branch: BranchPlan,
    added_worktree: PathBuf,
    init_submodules: bool,
}

impl AttachPlan {
    /// The directory the session ends up working in.
    pub fn workspace_dir(&self) -> &Path {
        &self.workspace_dir
    }
}

/// Do the filesystem work for a validated plan, without persisting anything.
///
/// The caller must already have quiesced the session when
/// [`AttachPlan::moves_session`] is set.
pub fn execute(instance: &super::Instance, plan: AttachPlan) -> Result<PreparedAttach> {
    let AttachPlan {
        conversion,
        workspace_dir,
        added_name: repo_name,
        added_main_repo: main_repo_path,
        added_branch: plan,
        added_worktree: worktree_path,
        init_submodules,
        ..
    } = plan;
    let git_wt = GitWorktree::new(main_repo_path.clone())?.with_init_submodules(init_submodules);

    // Order matters for rollback: the workspace directory first (so there is
    // something to clean up), then the session's own repo, then the new one. The
    // primary step is the one that can move user data, so it happens before the
    // added repo's worktree, where a failure has less to undo.
    let created_dir = !workspace_dir.exists();
    std::fs::create_dir_all(&workspace_dir)
        .with_context(|| format!("could not create the workspace {}", workspace_dir.display()))?;

    let mut undo = Undo {
        workspace_dir: created_dir.then(|| workspace_dir.clone()),
        ..Undo::default()
    };

    // `moved_to` is the session's new working directory, which is the workspace
    // root, not the primary's worktree inside it: that is where a session created
    // multi-repo starts, and it is what `attach_planned` persists as
    // `project_path`. `None` for Append, which is also what marks the attach as
    // "nothing moved" for every caller.
    let moved_to = (!matches!(conversion, Conversion::Append { .. }))
        .then(|| workspace_dir.to_string_lossy().to_string());

    let primary = match &conversion {
        Conversion::Append { .. } => None,
        Conversion::MoveIn { primary, from, .. } => {
            let to = PathBuf::from(&primary.worktree_path);
            let primary_git = GitWorktree::new(PathBuf::from(&primary.main_repo_path))?;
            if let Err(e) = primary_git.move_worktree(from, &to) {
                undo.run();
                return Err(e).with_context(|| {
                    format!(
                        "could not move this session's worktree into {}",
                        workspace_dir.display()
                    )
                });
            }
            undo.moved_primary = Some((primary.main_repo_path.clone(), to, from.clone()));
            Some(primary.clone())
        }
        Conversion::WorktreePrimary {
            primary,
            create_branch,
            base,
            ..
        } => {
            let to = PathBuf::from(&primary.worktree_path);
            let primary_git = GitWorktree::new(PathBuf::from(&primary.main_repo_path))?
                .with_init_submodules(init_submodules);
            if let Err(e) =
                primary_git.create_worktree(&primary.branch, &to, *create_branch, base.as_deref())
            {
                undo.run();
                return Err(e).with_context(|| {
                    format!(
                        "could not create a worktree for this session's own repo in {}",
                        workspace_dir.display()
                    )
                });
            }
            undo.created_primary = Some((
                primary.main_repo_path.clone(),
                to,
                create_branch.then(|| primary.branch.clone()),
            ));
            Some(primary.clone())
        }
    };

    let warnings = match git_wt.create_worktree(
        &plan.branch,
        &worktree_path,
        plan.create,
        plan.base.as_deref(),
    ) {
        Ok(w) => w,
        Err(e) => {
            undo.run();
            return Err(e)
                .with_context(|| format!("could not create a worktree for '{repo_name}'"));
        }
    };
    undo.added = Some((
        main_repo_path.to_string_lossy().to_string(),
        worktree_path.clone(),
        plan.create.then(|| plan.branch.clone()),
    ));

    let repo = WorkspaceRepo {
        name: repo_name,
        source_path: main_repo_path.to_string_lossy().to_string(),
        branch: plan.branch.clone(),
        worktree_path: worktree_path.to_string_lossy().to_string(),
        main_repo_path: main_repo_path.to_string_lossy().to_string(),
        managed_by_aoe: true,
        // True when the branch was already there and the caller opted into
        // reusing it, so deleting the session leaves the user's branch alone.
        branch_preexisting: !plan.create,
    };

    // The repo list the session ends up with: whatever it already had, then the
    // session's own repo when this converted it, then the new one.
    let mut repos: Vec<WorkspaceRepo> = instance.all_repos().to_vec();
    repos.extend(primary);
    repos.push(repo.clone());

    let workspace_info = WorkspaceInfo {
        branch: session_branch(instance)
            .map(str::to_string)
            .unwrap_or_else(|| repos[0].branch.clone()),
        workspace_dir: workspace_dir.to_string_lossy().to_string(),
        repos,
        created_at: instance
            .workspace_info
            .as_ref()
            .map(|ws| ws.created_at)
            .unwrap_or_else(Utc::now),
        cleanup_on_delete: true,
    };

    Ok(PreparedAttach {
        outcome: AttachOutcome {
            repo,
            warnings,
            moved_to,
            workspace_info: workspace_info.clone(),
        },
        workspace_info,
        undo,
    })
}

/// Filesystem work done by [`execute`], in the order it has to be undone.
///
/// Each field is `Some` only once its step actually succeeded, so [`Self::run`]
/// is safe to call at any point during the sequence.
#[derive(Default)]
struct Undo {
    /// Only set when [`execute`] created it, so appending to an existing workspace
    /// never removes the directory the session already lives in.
    workspace_dir: Option<PathBuf>,
    /// `(main_repo, moved_to, move_back_to)`.
    moved_primary: Option<(String, PathBuf, PathBuf)>,
    /// `(main_repo, worktree, branch_to_delete)`.
    created_primary: Option<(String, PathBuf, Option<String>)>,
    /// `(main_repo, worktree, branch_to_delete)`.
    added: Option<(String, PathBuf, Option<String>)>,
}

impl Undo {
    /// Best effort throughout: the original failure is the error worth
    /// reporting, and a leftover worktree is recoverable with
    /// `aoe worktree cleanup`. Reverse order of creation, so the workspace
    /// directory is only removed once its contents are gone.
    fn run(&self) {
        for (main_repo, worktree, branch) in [self.added.as_ref(), self.created_primary.as_ref()]
            .into_iter()
            .flatten()
        {
            if let Ok(git) = GitWorktree::new(PathBuf::from(main_repo)) {
                let _ = git.remove_worktree(worktree, true);
                if let Some(branch) = branch {
                    let _ = git.delete_branch(branch);
                }
            }
        }
        // Putting the session's own worktree back is the one step that matters
        // for user data: until it lands, `project_path` names a directory that
        // does not exist.
        if let Some((main_repo, from, back_to)) = &self.moved_primary {
            match GitWorktree::new(PathBuf::from(main_repo)) {
                Ok(git) => {
                    if let Err(e) = git.move_worktree(from, back_to) {
                        tracing::error!(
                            target: "session.attach",
                            from = %from.display(),
                            to = %back_to.display(),
                            "could not move the session's worktree back after a failed attach: {e:#}"
                        );
                    }
                }
                Err(e) => tracing::error!(
                    target: "session.attach",
                    "could not open {main_repo} to move the session's worktree back: {e:#}"
                ),
            }
        }
        if let Some(dir) = &self.workspace_dir {
            // `remove_dir`, not `remove_dir_all`: if anything is still in there
            // the removal must fail loudly rather than take it with us.
            let _ = std::fs::remove_dir(dir);
        }
    }
}

/// A created worktree that has not been recorded on the session yet.
///
/// Holds what [`Self::rollback`] needs, so a caller whose persist fails can
/// undo the filesystem work and leave no orphan behind.
pub struct PreparedAttach {
    pub outcome: AttachOutcome,
    /// The workspace the session becomes, ready for the caller to persist.
    pub workspace_info: WorkspaceInfo,
    undo: Undo,
}

impl PreparedAttach {
    /// Undo every filesystem change this attach made.
    ///
    /// For the caller whose own persist failed: without this, a session record
    /// could still name the old `project_path` while the worktree has already
    /// moved into the workspace.
    pub fn rollback(&self) {
        self.undo.run();
    }

    /// Where the session's working directory ends up, for the caller to persist
    /// alongside `workspace_info`.
    pub fn project_path(&self) -> &str {
        &self.workspace_info.workspace_dir
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

    let plan = plan(instance, profile, repo_path, on_existing)?;
    attach_planned(storage, session_id, instance, plan)
}

/// Execute an already-validated plan and persist it.
///
/// Split out so a caller that has to quiesce the session can do so *between*
/// [`plan`] and here: the moving shapes need the worker stopped and the sandbox
/// container removed before anything is renamed, and validating first means a
/// refusal never costs the user a stopped session. The daemon needs this split
/// because its quiesce is async and cannot run inside a blocking closure.
pub fn attach_planned(
    storage: &Storage,
    session_id: &str,
    instance: &super::Instance,
    plan: AttachPlan,
) -> Result<AttachOutcome> {
    let prepared = execute(instance, plan)?;

    let id = session_id.to_string();
    let workspace = prepared.workspace_info.clone();
    let new_project_path = prepared.project_path().to_string();
    let converted = prepared.outcome.moved_to.is_some();
    let persisted = storage.update(|instances, _groups| {
        let inst = instances
            .iter_mut()
            .find(|i| i.id == id)
            .with_context(|| format!("session not found: {id}"))?;
        inst.workspace_info = Some(workspace);
        if converted {
            // The session now works in the workspace directory, and its old
            // single-repo worktree record is superseded by the entry for that
            // same repo inside `workspace_info.repos`. Leaving `worktree_info`
            // set would have the delete path handle the primary worktree twice.
            inst.project_path = new_project_path;
            inst.worktree_info = None;
        }
        Ok(())
    });

    if let Err(e) = persisted {
        prepared.rollback();
        return Err(e).with_context(|| {
            format!(
                "could not record the attached repo; undid the worktree at {}",
                prepared.outcome.repo.worktree_path
            )
        });
    }

    Ok(prepared.outcome)
}

/// Whether an attach has to stop the session before it can land.
///
/// Only the shape that leaves the session exactly where it is, in a container
/// whose mount set does not change, needs nothing stopped: the new worktree just
/// appears inside the directory the agent is already working in.
pub fn needs_restart(plan: &AttachPlan, is_sandboxed: bool) -> bool {
    // A sandboxed session always does: the container's mounts are baked in at
    // creation, and `compute_workspace_volume_paths` mounts the workspace dir
    // plus each main repo individually, so a repo from elsewhere on disk adds a
    // mount even when nothing moves. (The common ancestor is only used to derive
    // container-side relative paths, not as the mount root.)
    plan.moves_session || is_sandboxed
}

/// What [`quiesce_for_conversion`] took down, so [`resume_after_conversion`]
/// puts back exactly that and nothing else.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Quiesced {
    /// A structured worker was signalled to stop. Its restart marker is
    /// deliberately not written here; see [`quiesce_for_conversion`].
    pub worker_was_running: bool,
    /// The tmux session was killed, so the pane has to be recreated. Recreated
    /// rather than left alone because the pane's shell (and the agent in it) was
    /// launched in the directory the conversion moves.
    pub pane_was_live: bool,
}

/// Stop everything holding the session's current working directory.
///
/// Order is load-bearing. The worker and the pane come down first, because
/// removing a container out from under a live agent kills it mid-turn; the
/// container comes down last, because its bind mount is what makes `rename(2)`
/// on the worktree fail `EBUSY`.
///
/// No restart marker is written here, unlike `aoe acp restart`. The marker is
/// what makes the daemon's reconciler respawn the worker, and a respawn between
/// here and the persist would come up in the directory the conversion is about
/// to move out from under it. [`resume_after_conversion`] writes it once the new
/// path is durable, and the reconciler honours a marker that arrives that late.
///
/// BLOCKING: kills a tmux session and shells out to `docker rm`.
pub fn quiesce_for_conversion(storage: &Storage, instance: &super::Instance) -> Result<Quiesced> {
    let mut quiesced = Quiesced::default();

    // The worker registry only exists in a build with the structured view, and
    // without it there is no ACP worker to stop.
    #[cfg(feature = "serve")]
    if let Ok(Some(record)) = crate::process::worker_registry::load(&instance.id) {
        crate::process::worker_registry::delete(&instance.id).ok();
        crate::process::worker::terminate_process_group(record.pid);
        quiesced.worker_was_running = true;
    }

    if instance.tmux_session().is_ok_and(|s| s.exists()) {
        instance.kill_clean().with_context(|| {
            format!(
                "could not stop '{}' before moving it into a workspace",
                instance.title
            )
        })?;
        quiesced.pane_was_live = true;
    }

    reset_sandbox_container(storage, &instance.id, instance.is_sandboxed())?;
    Ok(quiesced)
}

/// Start the session again, in whatever directory it now has.
///
/// Reads the instance back from disk rather than taking one from the caller: by
/// this point the persist has moved `project_path`, and the start cascade has to
/// use the new one. Returns warnings rather than failing, because the repo is
/// already attached and durable; a session that did not come back up is
/// restartable from the session list.
pub fn resume_after_conversion(
    storage: &Storage,
    session_id: &str,
    quiesced: Quiesced,
) -> Vec<String> {
    let mut warnings = Vec::new();

    if quiesced.pane_was_live {
        match storage
            .load()
            .ok()
            .and_then(|all| all.into_iter().find(|i| i.id == session_id))
        {
            Some(instance) => {
                let before = instance.clone();
                let result = super::restart::perform_restart(super::restart::RestartRequest {
                    session_id: session_id.to_string(),
                    instance,
                    size: None,
                    // No wake-up keys. From the user's point of view the session
                    // moved rather than restarted, and an unsolicited prompt
                    // would start a turn nobody asked for.
                    wake_message: String::new(),
                });
                match result.outcome {
                    Ok(_) => {
                        let after = *result.instance;
                        let id = session_id.to_string();
                        // The same compare-and-swap merge the TUI's restart
                        // poller uses, so the cascade's mutations (container id,
                        // cleared stale agent session id) land without
                        // clobbering a peer's concurrent edit.
                        if let Err(e) = storage.update(|instances, _groups| {
                            if let Some(slot) = instances.iter_mut().find(|i| i.id == id) {
                                slot.merge_post_restart_with_baseline(&before, &after);
                            }
                            Ok(())
                        }) {
                            warnings.push(format!(
                                "the session restarted but its record could not be updated ({e:#})"
                            ));
                        }
                    }
                    Err(e) => warnings.push(format!(
                        "the session could not be started again in its new directory ({e}); \
                         start it from the session list"
                    )),
                }
            }
            None => warnings
                .push("the session disappeared before it could be started again".to_string()),
        }
    }

    #[cfg(feature = "serve")]
    if quiesced.worker_was_running {
        crate::process::worker_registry::mark_restart_pending(session_id);
    }

    warnings
}

/// A TUI-initiated attach, handed to a background worker thread.
///
/// The lifecycle and duplicate checks stay on the caller's side, where the
/// in-memory instance already is and a refusal can be shown immediately; this
/// carries only what the blocking half needs.
pub struct AttachProjectRequest {
    pub session_id: String,
    pub profile: String,
    pub repo_path: PathBuf,
    /// Snapshotted by the caller so the worker does not have to re-derive it,
    /// and so the container reset is skipped without a `docker` call.
    pub is_sandboxed: bool,
}

/// Result of [`perform_attach_project`], already phrased for the user.
pub struct AttachProjectResult {
    pub session_id: String,
    /// `Ok` carries the success notice, including any restart or container
    /// warning; `Err` carries the refusal or failure.
    pub outcome: Result<String, String>,
}

/// Everything about an attach that must not run on the TUI render thread.
///
/// `git worktree add` alone can take seconds, and with a fetch or submodule init
/// behind it longer; the persist, the stop and the restart add more. Running
/// these inline froze the UI for the whole attach, which is what the TUI's
/// `attach_project_poller` exists to avoid.
pub fn perform_attach_project(request: AttachProjectRequest) -> AttachProjectResult {
    let session_id = request.session_id.clone();
    let outcome = attach_and_restart(request);
    AttachProjectResult {
        session_id,
        outcome,
    }
}

/// Plan, stop, convert, start again.
///
/// The three phases are separated so a refusal never costs the user a stopped
/// session: everything that can reject the attach happens in [`plan`], with
/// nothing written and nothing stopped.
fn attach_and_restart(request: AttachProjectRequest) -> Result<String, String> {
    let storage = Storage::open_unwatched(&request.profile).map_err(|e| format!("{e:#}"))?;
    let instances = storage.load().map_err(|e| format!("{e:#}"))?;
    let instance = instances
        .iter()
        .find(|i| i.id == request.session_id)
        .ok_or_else(|| format!("session not found: {}", request.session_id))?;

    let plan = plan(
        instance,
        &request.profile,
        &request.repo_path,
        // The TUI picker has no place to confirm reusing a branch, so it takes
        // the safe path and refuses; `aoe session add-project
        // --attach-existing-branch` is the way to opt in.
        ExistingBranch::Refuse,
    )
    .map_err(|e| format!("{e:#}"))?;

    let restarts = needs_restart(&plan, request.is_sandboxed);
    let quiesced = if restarts {
        quiesce_for_conversion(&storage, instance).map_err(|e| format!("{e:#}"))?
    } else {
        Quiesced::default()
    };

    let outcome = match attach_planned(&storage, &request.session_id, instance, plan) {
        Ok(outcome) => outcome,
        Err(e) => {
            // The session was stopped for an attach that then failed. Put it
            // back: the rollback already undid the filesystem half, so leaving
            // it down would be the only lasting damage.
            resume_after_conversion(&storage, &request.session_id, quiesced);
            return Err(format!("{e:#}"));
        }
    };

    let mut message = format!(
        "Attached '{}' on branch '{}'.",
        outcome.repo.name, outcome.repo.branch
    );
    if let Some(moved_to) = &outcome.moved_to {
        message.push_str(&format!(
            "\n\nThis session is now a multi-repo workspace; its working directory moved to \
             {moved_to}."
        ));
    }
    for warning in &outcome.warnings {
        message.push_str(&format!("\n\nWarning: {warning}"));
    }

    if restarts {
        message.push_str("\n\nRestarted the session so it comes up with the new repo");
        if quiesced.worker_was_running {
            message.push_str("; the conversation is preserved.");
        } else {
            message.push('.');
        }
    } else {
        message.push_str(
            "\n\nThe agent is already working in this directory, so nothing was restarted.",
        );
    }
    for warning in resume_after_conversion(&storage, &request.session_id, quiesced) {
        message.push_str(&format!("\n\nWarning: {warning}"));
    }

    Ok(message)
}

/// Drop a sandbox session's container so its next start mounts the new repo.
///
/// A container's bind mounts are fixed at `docker run`, and
/// [`super::Instance::get_container_for_instance`] reuses an existing container
/// by name: a stopped one is simply started again. Nothing short of removing it
/// changes the mount set, so without this the agent comes back up in a container
/// that has no idea the repo was attached. `discard` keeps the session's named
/// cache volumes, and clearing the create-time pins lets the workdir and
/// container id be re-derived against the new set.
///
/// Every surface needs it, so it lives here rather than in the daemon: the CLI
/// and the TUI bounce their worker through the registry and would otherwise
/// restart it into the stale container.
///
/// A no-op when `is_sandboxed` is false, so an unsandboxed session pays no
/// `docker` subprocess. Errors from the pin clear are logged rather than
/// returned: the removal is what makes the next start correct, and a stale pin
/// on disk is re-derived on the next create anyway.
///
/// BLOCKING: shells out to `docker rm`. Callers on the TUI thread already block
/// on `git worktree add` in [`execute`], so this adds no new class of stall, but
/// an async caller must still run it on a blocking thread.
///
/// The caller must have stopped any worker running inside the container first.
/// Removing a container out from under a live agent kills it mid-turn.
pub fn reset_sandbox_container(
    storage: &Storage,
    session_id: &str,
    is_sandboxed: bool,
) -> Result<()> {
    if !is_sandboxed {
        return Ok(());
    }

    match crate::containers::DockerContainer::from_session_id(session_id).discard() {
        crate::containers::Teardown::Removed => tracing::info!(
            target: "containers.runtime",
            session = %session_id,
            "removed the sandbox container after attaching a repo; it is recreated with the new mount set on next start"
        ),
        crate::containers::Teardown::AlreadyGone => {}
        crate::containers::Teardown::Failed(e) => {
            bail!("could not remove the old container: {e}")
        }
    }

    let id = session_id.to_string();
    let cleared = storage.update(|instances, _groups| {
        if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
            if let Some(sandbox) = inst.sandbox_info.as_mut() {
                sandbox.container_id = None;
                sandbox.container_workdir = None;
            }
        }
        Ok(())
    });
    if let Err(e) = cleared {
        tracing::warn!(
            target: "containers.runtime",
            session = %session_id,
            "could not clear the container pins after attaching a repo: {e:#}"
        );
    }
    Ok(())
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
                branch_preexisting: false,
            }],
            created_at: Utc::now(),
            cleanup_on_delete: true,
        });
        inst
    }

    /// A scratch session's cwd is a throwaway directory under the app dir, so
    /// there is no repo for an attached one to sit beside and deletion drops the
    /// whole tree. Refused at the shared choke point rather than per surface, so
    /// the CLI and the REST endpoint cannot reach it behind the pickers' backs.
    /// The path here is not a repo either: the assertion is that the scratch
    /// refusal wins, so the user is told the real reason.
    #[test]
    fn plan_refuses_a_scratch_session() {
        let mut inst = Instance::new("Scratchpad", "/tmp/scratch/abc");
        inst.scratch = true;
        let Err(err) = plan(
            &inst,
            "default",
            Path::new("/tmp/definitely-not-a-repo"),
            ExistingBranch::Refuse,
        ) else {
            panic!("a scratch session has no repo to attach to");
        };
        let msg = format!("{err:#}");
        assert!(
            msg.contains("scratch session"),
            "the scratch refusal must win over the not-a-git-repo error: {msg}"
        );
    }

    /// The lifecycle refusals live at the shared choke point, so the CLI and the
    /// REST endpoint cannot attach into a window the pickers already refuse.
    /// `Deleting` is the one with teeth: the deletion pass has already read the
    /// repo list, so a worktree created here is orphaned with its record about
    /// to be dropped. The path is not a repo either, which is the point:
    /// each lifecycle refusal has to win over the not-a-git-repo error so the user
    /// is told the real reason.
    #[test]
    fn plan_refuses_states_that_are_never_attachable() {
        let attempt = |inst: &Instance| {
            let Err(err) = plan(
                inst,
                "default",
                Path::new("/tmp/definitely-not-a-repo"),
                ExistingBranch::Refuse,
            ) else {
                panic!("this lifecycle state must be refused");
            };
            format!("{err:#}")
        };

        for status in [
            super::super::Status::Creating,
            super::super::Status::Deleting,
        ] {
            let mut inst = Instance::new("Busy", "/tmp/busy");
            inst.status = status;
            let msg = attempt(&inst);
            assert!(
                msg.contains("being created or is being deleted"),
                "{status:?} must be refused with its own reason: {msg}"
            );
        }

        let mut trashed = Instance::new("Trashed", "/tmp/trashed");
        trashed.trashed_at = Some(Utc::now());
        assert!(attempt(&trashed).contains("in the trash"));

        let mut archived = Instance::new("Archived", "/tmp/archived");
        archived.archived_at = Some(Utc::now());
        assert!(attempt(&archived).contains("archived"));

        // `Running` is deliberately allowed through: the daemon decides it on the
        // in-flight-turn probe, so gating it here would make that check coarser.
        // It falls through to the not-a-git-repo error instead.
        let mut running = Instance::new("Running", "/tmp/running");
        running.status = super::super::Status::Running;
        assert!(
            attempt(&running).contains("not a git repository"),
            "a Running session must reach the repo checks, not a lifecycle refusal"
        );
    }

    /// The `is_sandboxed` short-circuit is load-bearing, not a micro-optimisation:
    /// every unsandboxed attach goes through here, and without it each one shells
    /// out to `docker rm` (and fails the attach's warning path on a host with no
    /// container runtime at all). Passing a session id that has no container and
    /// asserting `Ok` proves no runtime call is attempted.
    #[test]
    #[serial_test::serial]
    fn reset_sandbox_container_is_a_no_op_without_a_sandbox() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        crate::session::create_profile("attach-noop").expect("profile");
        let storage = Storage::open_unwatched("attach-noop").expect("storage");
        assert!(
            reset_sandbox_container(&storage, "no-such-session", false).is_ok(),
            "an unsandboxed session must not touch the container runtime"
        );
    }

    fn git_in(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {args:?} in {} failed: {}",
            dir.display(),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A repo with one commit, so branches and worktrees can be created.
    fn init_repo(path: &Path) {
        std::fs::create_dir_all(path).expect("create repo dir");
        git_in(path, &["init", "-q"]);
        git_in(path, &["config", "user.email", "test@example.com"]);
        git_in(path, &["config", "user.name", "Test"]);
        std::fs::write(path.join("README.md"), "x").expect("seed file");
        git_in(path, &["add", "."]);
        git_in(path, &["commit", "-qm", "init"]);
    }

    /// An isolated app dir plus an empty profile, so the config and base-branch
    /// lookups `plan` does resolve against test state rather than the developer's.
    fn isolated_profile(temp: &Path, name: &str) -> crate::session::test_support::AppDirGuard {
        let guard = crate::session::test_support::isolate_app_dir_at(temp);
        crate::session::create_profile(name).expect("profile");
        guard
    }

    /// A session that is already a workspace gains the new repo inside the
    /// workspace it has. Nothing moves, which is the one shape that needs no
    /// stop-and-start, so `moves_session` has to stay false and the recorded
    /// `project_path` has to be left for the caller to keep.
    #[test]
    #[serial_test::serial]
    fn appending_to_an_existing_workspace_moves_nothing() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = isolated_profile(temp.path(), "attach-append");

        let backend = temp.path().join("src/backend");
        let frontend = temp.path().join("src/frontend");
        let workspace = temp.path().join("ws");
        init_repo(&backend);
        init_repo(&frontend);
        std::fs::create_dir_all(&workspace).unwrap();
        let backend_wt = workspace.join("backend");
        git_in(
            &backend,
            &[
                "worktree",
                "add",
                "-b",
                "featx",
                backend_wt.to_str().unwrap(),
            ],
        );

        let mut inst = Instance::new("Workspace", workspace.to_str().unwrap());
        inst.workspace_info = Some(WorkspaceInfo {
            branch: "featx".to_string(),
            workspace_dir: workspace.to_string_lossy().to_string(),
            repos: vec![WorkspaceRepo {
                name: "backend".to_string(),
                source_path: backend.to_string_lossy().to_string(),
                branch: "featx".to_string(),
                worktree_path: backend_wt.to_string_lossy().to_string(),
                main_repo_path: backend.to_string_lossy().to_string(),
                managed_by_aoe: true,
                branch_preexisting: false,
            }],
            created_at: Utc::now(),
            cleanup_on_delete: true,
        });

        let plan = plan(&inst, "attach-append", &frontend, ExistingBranch::Refuse)
            .expect("attaching to a workspace session must be accepted");
        assert!(
            !plan.moves_session,
            "an existing workspace is not moved, so nothing has to be stopped"
        );
        assert_eq!(plan.workspace_dir(), workspace);

        let prepared = execute(&inst, plan).expect("the worktree must be created");
        assert!(
            prepared.outcome.moved_to.is_none(),
            "nothing moved, so there is no new project_path to report"
        );
        assert!(workspace.join("frontend/.git").exists());
        assert!(
            backend_wt.join(".git").exists(),
            "the repo the session already had must be untouched"
        );
        assert_eq!(
            prepared
                .workspace_info
                .repos
                .iter()
                .map(|r| r.name.as_str())
                .collect::<Vec<_>>(),
            vec!["backend", "frontend"]
        );
    }

    /// The in-place shape moves the session's working directory into a new
    /// workspace, and a fresh worktree of the session's own repo cannot carry
    /// uncommitted work with it. Refused rather than silently leaving that work
    /// outside the session.
    #[test]
    #[serial_test::serial]
    fn a_dirty_in_place_checkout_is_refused() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = isolated_profile(temp.path(), "attach-dirty");

        let backend = temp.path().join("src/backend");
        let frontend = temp.path().join("src/frontend");
        init_repo(&backend);
        init_repo(&frontend);
        std::fs::write(backend.join("wip.txt"), "unsaved").unwrap();

        let inst = Instance::new("Dirty Session", backend.to_str().unwrap());
        let Err(err) = plan(&inst, "attach-dirty", &frontend, ExistingBranch::Refuse) else {
            panic!("a dirty in-place checkout must be refused");
        };
        let msg = format!("{err:#}");
        assert!(
            msg.contains("uncommitted changes") && msg.contains("Commit or stash"),
            "the refusal has to say what to do about it: {msg}"
        );
        assert_eq!(
            std::fs::read_to_string(backend.join("wip.txt")).unwrap(),
            "unsaved",
            "a refusal must not touch the checkout"
        );
    }

    /// The rollback that matters: for a worktree session the primary is *moved*
    /// into the new workspace, so a later failure leaves `project_path` naming a
    /// directory that no longer exists unless the move is undone. Forced by
    /// occupying the added repo's target between `plan` and `execute`, which is
    /// exactly the window the two-phase split opens.
    #[test]
    #[serial_test::serial]
    fn a_failed_attach_moves_the_sessions_worktree_back() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = isolated_profile(temp.path(), "attach-rollback");

        let backend = temp.path().join("src/backend");
        let frontend = temp.path().join("src/frontend");
        init_repo(&backend);
        init_repo(&frontend);
        let session_wt = temp.path().join("src/backend-featx");
        git_in(
            &backend,
            &[
                "worktree",
                "add",
                "-b",
                "featx",
                session_wt.to_str().unwrap(),
            ],
        );
        std::fs::write(session_wt.join("wip.txt"), "in progress").unwrap();

        let mut inst = Instance::new("Worktree Session", session_wt.to_str().unwrap());
        inst.worktree_info = Some(WorktreeInfo {
            branch: "featx".to_string(),
            main_repo_path: backend.to_string_lossy().to_string(),
            managed_by_aoe: true,
            created_at: Utc::now(),
            base_branch: None,
        });

        let plan = plan(&inst, "attach-rollback", &frontend, ExistingBranch::Refuse)
            .expect("the attach itself is valid");
        assert!(
            plan.moves_session,
            "a worktree session's directory moves, so the caller has to stop it"
        );

        // Occupy the added repo's target so `git worktree add` fails after the
        // session's own worktree has already been moved in.
        let blocker = plan.workspace_dir().join("frontend");
        std::fs::create_dir_all(&blocker).unwrap();
        std::fs::write(blocker.join("in-the-way.txt"), "x").unwrap();

        let Err(err) = execute(&inst, plan) else {
            panic!("the added repo's worktree cannot be created over a non-empty directory");
        };
        assert!(
            format!("{err:#}").contains("frontend"),
            "the error should name the repo that failed: {err:#}"
        );
        assert!(
            session_wt.join("wip.txt").exists(),
            "the session's worktree must be moved back, with its uncommitted work"
        );
        assert_eq!(
            std::fs::read_to_string(session_wt.join("wip.txt")).unwrap(),
            "in progress"
        );
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
