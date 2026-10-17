//! Attach a repo to a session that already exists, by converting it into a multi-repo workspace.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::Utc;

use super::builder;
use super::claim::ResourceReservation;
use super::instance::{WorkspaceInfo, WorkspaceRepo};
use super::move_journal::{
    EffectObservation, IntentOwner, ProfileAuthority, ResourceEffect, ResourceIntent,
};
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
    /// Set when the session was converted into a workspace, carrying its new `project_path`.
    pub moved_to: Option<String>,
    /// The workspace the session now has, as persisted.
    pub workspace_info: WorkspaceInfo,
}

/// The directory leaf an attached repo is known by.
fn repo_leaf_name(main_repo_path: &Path) -> String {
    main_repo_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "repo".to_string())
}

/// Reject an attach that duplicates a repo the session already has.
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

/// Best-effort canonicalization for identity comparison.
fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// The branch a session's worktrees are on, if it has one.
fn session_branch(instance: &super::Instance) -> Option<&str> {
    instance
        .worktree_info
        .as_ref()
        .map(|w| w.branch.as_str())
        .or_else(|| instance.workspace_info.as_ref().map(|w| w.branch.as_str()))
}

/// The branch to create in a repo attached to a session that has none of its own (a plain in-place
/// session).
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
#[derive(Clone)]
struct BranchPlan {
    branch: String,
    create: bool,
    base: Option<String>,
}

/// Decide the branch for the attached repo.
fn plan_branch(
    git_wt: &GitWorktree,
    suggested: &str,
    base: Option<String>,
    on_existing: ExistingBranch,
) -> Result<BranchPlan> {
    let branch = builder::git_sanitize_branch_name(suggested);
    // Checked immediately before `create_worktree` runs, so a branch created between here and there
    // surfaces as a git error rather than being silently reused.
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

/// Refuse when the resolved branch is already checked out in another worktree of the added repo.
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
#[derive(Clone)]
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
/// Whether the next launch resumes a known conversation that cannot move.
fn conversation_cannot_follow(instance: &super::Instance) -> bool {
    instance
        .conversation_target()
        .and_then(|(_, binding, _)| binding)
        .is_some_and(crate::session::ConversationBinding::is_known)
}

/// Decide how to make room for the new repo, and where the workspace lands.
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

    // The session's own repo, whichever shape it is in.
    let current = PathBuf::from(&instance.project_path);
    let main_repo = match &instance.worktree_info {
        Some(wt) => canonical(Path::new(&wt.main_repo_path)),
        None => canonical(&GitWorktree::find_main_repo(&current)?),
    };
    let primary_name = repo_leaf_name(&main_repo);
    let git_wt = GitWorktree::new(main_repo.clone())?;
    let config = super::config::repo_config::resolve_config_with_repo_or_warn(profile, &main_repo);
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

    // Only an aoe-created worktree is ours to relocate.
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
                branch_preexisting: false,
                // Carry the existing worktree's recorded base and the session's own diff-base
                // override across the conversion.
                base_branch: wt.base_branch.clone(),
                base_branch_override: instance.base_branch_override.clone(),
            },
            from: current,
        });
    }

    // In-place, or a worktree the user created.
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
    // A fresh worktree cannot check out the branch the user's checkout already has, so the session
    // gets one derived from its title, planned against the same opt-in rule the added repo uses.
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
            base_branch: plan.create.then(|| plan.base.clone()).flatten(),
            // Same carry-forward as the MoveIn arm above: the session's own
            // override has no other home once it is a workspace member.
            base_branch_override: instance.base_branch_override.clone(),
        },
        create_branch: plan.create,
        base: plan.base,
    })
}

/// Validate an attach request without checkout or row mutations.
pub fn plan(
    instance: &super::Instance,
    profile: &str,
    repo_path: &Path,
    on_existing: ExistingBranch,
) -> Result<AttachPlan> {
    // A scratch session has no repo of its own: its cwd is `<app_dir>/scratch/ <id>/`, which
    // deletion removes wholesale.
    if instance.scratch {
        bail!(
            "'{}' is a scratch session, which has no repo to attach to. Create a session on the \
             repo instead.",
            instance.title
        );
    }

    // Lifecycle states that are never a legitimate moment to attach, on any surface.
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

    let profile = super::config::effective_profile(profile);
    let config =
        super::config::repo_config::resolve_config_with_repo_or_warn(&profile, &main_repo_path);
    let git_wt = GitWorktree::new(main_repo_path.clone())?
        .with_init_submodules(config.worktree.init_submodules);

    let base = builder::resolve_base_branch(
        None,
        builder::project_base_branches(&profile)
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

    // Plan the conversion before touching anything, so a refusal (dirty checkout, workspace path
    // taken, branch already checked out) happens with nothing created.
    let conversion = plan_conversion(instance, &profile, on_existing)?;
    if !matches!(conversion, Conversion::Append { .. }) && conversation_cannot_follow(instance) {
        bail!(
            "'{}' carries a conversation bound to its current working directory; \
             moving the session into '{}' would leave that conversation \
             unresumable. Keep its current directory, or explicitly clear the \
             resume target before attaching to start a new conversation.",
            instance.title,
            conversion.workspace_dir().display()
        );
    }

    let workspace_dir = conversion.workspace_dir().to_path_buf();
    let worktree_path = workspace_dir.join(&repo_name);
    if worktree_path.exists() {
        bail!(
            "{} already exists; remove it, or delete the session that owns it, before attaching",
            worktree_path.display()
        );
    }

    Ok(AttachPlan {
        original: instance.clone(),
        reservation: None,
        effect_count: 0,
        added_effect: 0,
        profile,
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
pub struct AttachPlan {
    original: super::Instance,
    reservation: Option<std::sync::Arc<ResourceReservation>>,
    effect_count: usize,
    added_effect: usize,
    profile: String,
    /// Whether conversion changes the working directory and requires a stop/restart.
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

fn validate_attach_row(row: &super::Instance, plan: &AttachPlan) -> Result<()> {
    anyhow::ensure!(
        same_checkout(row, &plan.original),
        "session checkout changed after attach was planned"
    );
    anyhow::ensure!(
        !row.is_trashed() && !row.is_archived(),
        "session is changing lifecycle"
    );
    anyhow::ensure!(
        !matches!(
            row.status,
            super::Status::Creating | super::Status::Deleting
        ),
        "session is changing lifecycle"
    );
    anyhow::ensure!(
        row.lifecycle_generation == plan.original.lifecycle_generation
            && row.lifecycle_reservation.is_none(),
        "session is changing lifecycle"
    );
    anyhow::ensure!(
        !plan.moves_session || !conversation_cannot_follow(row),
        "session conversation cannot follow conversion"
    );
    Ok(())
}

/// Admit the original row and every conversion path before stopping any runtime.
pub fn reserve_attach(storage: &Storage, plan: &mut AttachPlan) -> Result<super::Instance> {
    let root = super::storage::acquire_ownership_lock()?;
    let _identity = super::storage::acquire_session_identity_lock_with_ownership(&root)?;
    let _lifecycle =
        storage.acquire_instance_lifecycle_lock_with_ownership(&root, &plan.original.id)?;
    anyhow::ensure!(
        storage.profile() == plan.profile && plan.reservation.is_none(),
        "attach plan has another profile or was already admitted"
    );
    let current = storage
        .load_strict_for_worktree_ownership()?
        .into_iter()
        .find(|row| row.id == plan.original.id)
        .context("session disappeared before attach admission")?;
    validate_attach_row(&current, plan)?;
    validate_attach_paths(storage, &root, &current, plan)?;
    let mut reserved = current.clone();
    let generation = reserved.try_acquire_lifecycle_reservation(
        super::LifecycleOperation::Attach,
        super::Instance::LIFECYCLE_RESERVATION_TTL,
        Utc::now(),
    )?;
    let (after, effects, added_effect) = attach_effects(&current, plan, generation)?;
    let effect_count = effects.len();
    let profile = ProfileAuthority::capture(storage)?;
    let intent = ResourceIntent::new(
        IntentOwner {
            profile: profile.clone(),
            id: current.id.clone(),
            created_at: current.created_at,
            generation,
        },
        profile,
        super::LifecycleOperation::Attach,
        Some(current),
        Some(after),
        effects,
        Vec::new(),
    )?;
    let admitted = ResourceReservation::admit_with_ownership(storage, &root, intent, reserved)?;
    plan.reservation = Some(admitted.transaction);
    plan.effect_count = effect_count;
    plan.added_effect = added_effect;
    Ok(admitted.instance)
}

fn attach_effects(
    current: &super::Instance,
    plan: &AttachPlan,
    generation: u64,
) -> Result<(super::Instance, Vec<ResourceEffect>, usize)> {
    let mut effects = Vec::new();
    if current.is_sandboxed() {
        effects.push(ResourceEffect::ReleaseContainer);
    }
    if plan.moves_session {
        effects.push(ResourceEffect::CreateDirectory {
            path: plan.workspace_dir.clone(),
        });
    }
    if let Conversion::WorktreePrimary {
        primary,
        create_branch,
        base,
        ..
    } = &plan.conversion
    {
        effects.push(ResourceEffect::create_worktree(
            primary.main_repo_path.clone().into(),
            primary.worktree_path.clone().into(),
            primary.branch.clone(),
            *create_branch,
            base.clone(),
            plan.init_submodules,
        )?);
    }
    let added_effect = effects.len();
    effects.push(ResourceEffect::create_worktree(
        plan.added_main_repo.clone(),
        plan.added_worktree.clone(),
        plan.added_branch.branch.clone(),
        plan.added_branch.create,
        plan.added_branch.base.clone(),
        plan.init_submodules,
    )?);
    if let Conversion::MoveIn { primary, from, .. } = &plan.conversion {
        effects.push(ResourceEffect::MoveWorktree {
            repository: primary.main_repo_path.clone().into(),
            from: from.clone(),
            to: primary.worktree_path.clone().into(),
        });
    }
    let added = WorkspaceRepo {
        name: plan.added_name.clone(),
        source_path: plan.added_main_repo.to_string_lossy().into_owned(),
        branch: plan.added_branch.branch.clone(),
        worktree_path: plan.added_worktree.to_string_lossy().into_owned(),
        main_repo_path: plan.added_main_repo.to_string_lossy().into_owned(),
        managed_by_aoe: true,
        branch_preexisting: !plan.added_branch.create,
        base_branch: plan
            .added_branch
            .create
            .then(|| plan.added_branch.base.clone())
            .flatten(),
        base_branch_override: None,
    };
    let mut repos = current.all_repos().to_vec();
    match &plan.conversion {
        Conversion::MoveIn { primary, .. } | Conversion::WorktreePrimary { primary, .. } => {
            repos.push(primary.clone())
        }
        Conversion::Append { .. } => {}
    }
    repos.push(added);
    let mut after = current.clone();
    after.workspace_info = Some(WorkspaceInfo {
        branch: session_branch(current)
            .map(str::to_owned)
            .unwrap_or_else(|| repos[0].branch.clone()),
        workspace_dir: plan.workspace_dir.to_string_lossy().into_owned(),
        repos,
        created_at: current
            .workspace_info
            .as_ref()
            .map_or_else(Utc::now, |workspace| workspace.created_at),
        cleanup_on_delete: true,
    });
    if plan.moves_session {
        after.project_path = plan.workspace_dir.to_string_lossy().into_owned();
        after.worktree_info = None;
    }
    after.lifecycle_generation = generation;
    after.lifecycle_reservation = None;
    Ok((after, effects, added_effect))
}

fn validate_attach_paths(
    storage: &Storage,
    ownership: &super::storage::OwnershipGuard,
    current: &super::Instance,
    plan: &AttachPlan,
) -> Result<()> {
    let paths = super::deletion::paths_in_use_except_with_ownership(
        ownership,
        storage.profile(),
        &[&current.id],
    );
    anyhow::ensure!(
        !paths.covers(&plan.added_worktree),
        "attached worktree path is already owned or inventory is unreadable"
    );
    if plan.moves_session {
        anyhow::ensure!(
            !paths.covers(&plan.workspace_dir),
            "conversion path is already owned or inventory is unreadable"
        );
    }
    if matches!(plan.conversion, Conversion::MoveIn { .. }) {
        anyhow::ensure!(
            !paths.covers(Path::new(&current.project_path)),
            "primary move source is already owned or inventory is unreadable"
        );
    }
    match &plan.conversion {
        Conversion::MoveIn { primary, .. } | Conversion::WorktreePrimary { primary, .. } => {
            anyhow::ensure!(
                !paths.covers(Path::new(&primary.worktree_path)),
                "primary destination is already owned or inventory is unreadable"
            );
        }
        Conversion::Append { .. } => {}
    }
    if plan.added_branch.create {
        anyhow::ensure!(
            !super::deletion::branch_in_use_with_ownership(
                ownership,
                storage.profile(),
                &[&current.id],
                &plan.added_main_repo,
                &plan.added_branch.branch
            )?,
            "attached branch is already owned"
        );
    }
    Ok(())
}

pub(crate) fn same_checkout(left: &super::Instance, right: &super::Instance) -> bool {
    fn worktree(row: &super::Instance) -> Option<(&str, &str, bool)> {
        row.worktree_info.as_ref().map(|wt| {
            (
                wt.main_repo_path.as_str(),
                wt.branch.as_str(),
                wt.managed_by_aoe,
            )
        })
    }
    left.id == right.id
        && left.created_at == right.created_at
        && left.project_path == right.project_path
        && worktree(left) == worktree(right)
        && left
            .workspace_info
            .as_ref()
            .map(|ws| ws.workspace_dir.as_str())
            == right
                .workspace_info
                .as_ref()
                .map(|ws| ws.workspace_dir.as_str())
        && left
            .all_repos()
            .iter()
            .map(|repo| {
                (
                    &repo.worktree_path,
                    &repo.main_repo_path,
                    &repo.branch,
                    repo.managed_by_aoe,
                )
            })
            .eq(right.all_repos().iter().map(|repo| {
                (
                    &repo.worktree_path,
                    &repo.main_repo_path,
                    &repo.branch,
                    repo.managed_by_aoe,
                )
            }))
}

/// Attach `repo_path` to the session identified by `session_id`.
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

    let mut plan = plan(instance, profile, repo_path, on_existing)?;
    let instance = reserve_attach(storage, &mut plan)?;
    let quiesced = if needs_restart(&plan, instance.is_sandboxed()) {
        match quiesce_for_conversion(storage, &plan) {
            Ok(quiesced) => quiesced,
            Err(error) => return Err(cancel_attach(&plan, error)),
        }
    } else {
        Quiesced {
            attach_generation: instance.lifecycle_generation,
            ..Default::default()
        }
    };
    let result = attach_planned(storage, session_id, &instance, plan);
    resume_after_conversion(storage, &instance, quiesced, result.as_ref().ok());
    result
}

/// Execute an already-validated plan and persist it.
pub fn attach_planned(
    storage: &Storage,
    session_id: &str,
    instance: &super::Instance,
    plan: AttachPlan,
) -> Result<AttachOutcome> {
    let transaction = plan
        .reservation
        .as_ref()
        .context("attach was not admitted")?;
    anyhow::ensure!(
        storage.profile() == plan.profile && session_id == plan.original.id,
        "attach plan has another owner"
    );
    if plan.moves_session {
        match instance.flush_published_conversation(storage) {
            Some(crate::session::SidWrite::Applied | crate::session::SidWrite::OwnershipConflict) | None => {}
            Some(outcome) => return Err(cancel_attach(&plan, anyhow::anyhow!(
                "'{}' has an undrained conversation publication ({outcome:?}); drain it or clear the resume target before converting",
                instance.title))),
        }
    }
    let mut warnings = Vec::new();
    for index in 0..plan.effect_count {
        let effect = transaction.effect(index)?;
        let release = matches!(effect, ResourceEffect::ReleaseContainer);
        let outcome = transaction.run_effect(index, || {
            if release {
                match crate::containers::DockerContainer::from_session_id(session_id).discard() {
                    crate::containers::Teardown::Removed
                    | crate::containers::Teardown::AlreadyGone => Ok((
                        Vec::new(),
                        Some(EffectObservation {
                            container_released: Some(true),
                            ..Default::default()
                        }),
                    )),
                    crate::containers::Teardown::Failed(error) => Err(error.into()),
                }
            } else {
                effect.execute().map(|warnings| (warnings, None))
            }
        });
        match outcome {
            Ok(Ok(effect_warnings)) if index == plan.added_effect => warnings = effect_warnings,
            Ok(Ok(_)) => {}
            Ok(Err(error)) => return Err(attach_failure(transaction, error, true)),
            Err(error) => return Err(attach_failure(transaction, error, false)),
        }
        if release {
            let cleared = (|| -> Result<()> {
                let root = super::storage::acquire_ownership_lock()?;
                let _identity =
                    super::storage::acquire_session_identity_lock_with_ownership(&root)?;
                let _lifecycle =
                    storage.acquire_instance_lifecycle_lock_with_ownership(&root, session_id)?;
                storage.update_with_intent(&root, transaction.journal(), |rows, _| {
                    let row = rows
                        .iter_mut()
                        .find(|row| {
                            row.id == session_id
                                && row.created_at == instance.created_at
                                && row.lifecycle_reservation_is_owned(
                                    super::LifecycleOperation::Attach,
                                    instance.lifecycle_generation,
                                )
                                && row.lifecycle_reservation.as_ref().is_some_and(|lease| {
                                    lease.resource_intent.as_deref()
                                        == Some(transaction.journal().id())
                                })
                        })
                        .context("attach owner changed before clearing container pins")?;
                    if let Some(sandbox) = &mut row.sandbox_info {
                        sandbox.container_id = None;
                        sandbox.container_workdir = None;
                    }
                    Ok(())
                })
            })();
            if let Err(error) = cleared {
                return Err(attach_failure(transaction, error, false));
            }
        }
    }
    let committed = (|| {
        let root = super::storage::acquire_ownership_lock()?;
        let _identity = super::storage::acquire_session_identity_lock_with_ownership(&root)?;
        let _lifecycle =
            storage.acquire_instance_lifecycle_lock_with_ownership(&root, session_id)?;
        transaction
            .publish_with_ownership(&root, |rows, candidate| {
                anyhow::ensure!(
                    !plan.moves_session || !conversation_cannot_follow(candidate),
                    "session conversation cannot follow conversion"
                );
                if super::is_duplicate_session(
                    rows.iter(),
                    &candidate.title,
                    &candidate.project_path,
                    Some(candidate.id.as_str()),
                ) {
                    return Err(super::duplicate_session_error(&candidate.title));
                }
                Ok(())
            })?
            .context("attach publication removed its owner")
    })();
    let committed = match committed {
        Ok(row) => row,
        Err(error) => return Err(attach_failure(transaction, error, true)),
    };
    let workspace_info = committed
        .workspace_info
        .context("attached session has no workspace")?;
    let repo = workspace_info
        .repos
        .iter()
        .find(|repo| {
            Path::new(&repo.worktree_path) == plan.added_worktree
                && Path::new(&repo.main_repo_path) == plan.added_main_repo
        })
        .context("attached repository is missing from committed workspace")?
        .clone();
    Ok(AttachOutcome {
        repo,
        warnings,
        moved_to: plan.moves_session.then_some(committed.project_path),
        workspace_info,
    })
}

fn attach_failure(
    transaction: &ResourceReservation,
    error: anyhow::Error,
    compensate: bool,
) -> anyhow::Error {
    let cleanup = if compensate {
        match transaction.rollback() {
            Ok(()) => return error.context("Attach was rolled back"),
            Err(cleanup) => Some(format!("compensation could not finish: {cleanup:#}")),
        }
    } else {
        None
    };
    let path = &transaction.journal().path;
    match path.try_exists() {
        Ok(false) => {
            return error
                .context(cleanup.unwrap_or_else(|| "No attach intent file remains".to_owned()))
        }
        Err(check) => {
            return error.context(format!(
                "Could not inspect attach intent {}: {check:#}",
                path.display()
            ))
        }
        Ok(true) => {}
    }
    let reason = cleanup.unwrap_or_else(|| format!("{error:#}"));
    let recorded = super::storage::acquire_ownership_lock()
        .and_then(|root| transaction.note_conflict_with_ownership(&root, reason));
    match recorded {
        Ok(()) => error.context(format!("Attach intent retained at {}", path.display())),
        Err(recording) => error.context(format!(
            "Attach intent diagnostic failed at {}: {recording:#}",
            path.display()
        )),
    }
}

pub(crate) fn cancel_attach(plan: &AttachPlan, error: anyhow::Error) -> anyhow::Error {
    match &plan.reservation {
        Some(transaction) => attach_failure(transaction, error, true),
        None => error,
    }
}

/// Whether an attach has to stop the session before it can land.
pub fn needs_restart(plan: &AttachPlan, is_sandboxed: bool) -> bool {
    // A sandboxed session always does: the container's mounts are baked in at creation, and
    // `compute_workspace_volume_paths` mounts the workspace dir plus each main repo individually,
    // so a repo from elsewhere on disk adds a mount even when nothing moves.
    plan.moves_session || is_sandboxed
}

/// What [`quiesce_for_conversion`] took down, so [`resume_after_conversion`]
/// puts back exactly that and nothing else.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Quiesced {
    /// A structured worker was signalled to stop. Its restart marker is
    /// deliberately not written here; see [`quiesce_for_conversion`].
    pub worker_was_running: bool,
    /// Generation of the stopped worker, so the restart marker written after
    /// the move authorizes only that runner's respawn.
    pub worker_generation: u64,
    /// The tmux session was killed, so the pane has to be recreated.
    pub pane_was_live: bool,
    pub attach_generation: u64,
}

/// Stop everything holding the session's current working directory.
pub fn quiesce_for_conversion(storage: &Storage, plan: &AttachPlan) -> Result<Quiesced> {
    let instance = {
        let root = super::storage::acquire_ownership_lock()?;
        let _identity = super::storage::acquire_session_identity_lock_with_ownership(&root)?;
        let _lifecycle =
            storage.acquire_instance_lifecycle_lock_with_ownership(&root, &plan.original.id)?;
        let instance = storage
            .load_strict_for_worktree_ownership()?
            .into_iter()
            .find(|row| row.id == plan.original.id)
            .context("session disappeared before attach quiescence")?;
        let transaction = plan
            .reservation
            .as_ref()
            .context("attach quiescence was not reserved")?;
        let entry = transaction.journal().read()?;
        let super::move_journal::MoveAuthority::Resource { intent } = &entry.authority else {
            bail!("attach has no resource intent")
        };
        anyhow::ensure!(
            intent.row_is_reserved(
                &ProfileAuthority::capture(storage)?,
                &instance,
                transaction.journal().id()
            ),
            "attach owner changed before quiescence"
        );
        instance
    };
    let mut quiesced = Quiesced {
        attach_generation: instance.lifecycle_generation,
        ..Default::default()
    };
    let worker = crate::process::worker_registry::load(&instance.id)?;
    if let Some(record) = &worker {
        anyhow::ensure!(
            record.session_id == instance.id
                && record.cwd == Path::new(&instance.project_path)
                && record
                    .source_profile
                    .as_deref()
                    .is_none_or(|profile| profile == storage.profile()),
            "worker changed before attach quiescence"
        );
    }
    if instance
        .tmux_session()
        .is_ok_and(|session| session.exists())
    {
        instance.kill_locked().with_context(|| {
            format!(
                "could not stop '{}' before moving it into a workspace",
                instance.title
            )
        })?;
        quiesced.pane_was_live = true;
    }
    if let Some(record) = worker {
        crate::process::worker::terminate_process_group(record.pid);
        if let Err(error) = crate::process::worker_registry::delete(&instance.id) {
            tracing::warn!(target: "session.attach", "Stopped worker registry cleanup failed: {error:#}");
        }
        quiesced.worker_was_running = true;
        quiesced.worker_generation = record.generation;
    }
    Ok(quiesced)
}

pub(crate) fn resume_instance(
    storage: &Storage,
    original: &super::Instance,
    generation: u64,
    outcome: Option<&AttachOutcome>,
) -> Result<super::Instance> {
    let _ownership = super::storage::acquire_ownership_read()?;
    let row = storage
        .load_strict_for_worktree_ownership()?
        .into_iter()
        .find(|row| row.id == original.id)
        .context("session disappeared before attach restart")?;
    let mut expected = original.clone();
    if let Some(outcome) = outcome {
        expected.workspace_info = Some(outcome.workspace_info.clone());
        if let Some(path) = &outcome.moved_to {
            expected.project_path = path.clone();
            expected.worktree_info = None;
        }
    }
    anyhow::ensure!(
        same_checkout(&row, &expected)
            && row.lifecycle_generation == generation
            && row.lifecycle_reservation.is_none()
            && !row.is_trashed()
            && !row.is_archived(),
        "session changed before attach restart"
    );
    Ok(row)
}

/// Start the session again, in whatever directory it now has.
pub fn resume_after_conversion(
    storage: &Storage,
    original: &super::Instance,
    quiesced: Quiesced,
    outcome: Option<&AttachOutcome>,
) -> Vec<String> {
    let mut warnings = Vec::new();
    let session_id = original.id.as_str();
    if !quiesced.pane_was_live && !quiesced.worker_was_running {
        return warnings;
    }
    let fresh = match resume_instance(storage, original, quiesced.attach_generation, outcome) {
        Ok(row) => row,
        Err(error) => return vec![format!("attach restart refused: {error:#}")],
    };

    if quiesced.pane_was_live {
        let before = fresh.clone();
        let result = super::restart::perform_restart_with_storage(
            super::restart::RestartRequest {
                session_id: session_id.to_string(),
                instance: fresh,
                size: None,
                wake_message: String::new(),
                skip_on_launch: false,
                bound_hooks: true,
                discard_sandbox_container: false,
                conversation_carry: None,
            },
            storage,
        );
        match result.outcome {
            Ok(_) => {
                let after = *result.instance;
                // Merge only the restart-owned fields without clobbering concurrent identity edits.
                if let Err(error) = storage.update(|rows, _| {
                    if let Some(row) = rows.iter_mut().find(|row| row.id == session_id) {
                        row.merge_post_restart_with_baseline(&before, &after);
                    }
                    Ok(())
                }) {
                    warnings.push(format!("the session restarted but its record could not be updated ({error:#})"));
                }
            }
            Err(error) => warnings.push(format!(
                "the session could not be started again in its new directory ({error}); start it from the session list"
            )),
        }
    }

    if quiesced.worker_was_running {
        let marker = (|| -> Result<()> {
            let _ownership = super::storage::acquire_ownership_read()?;
            let generation = quiesced
                .attach_generation
                .checked_add(u64::from(
                    quiesced.pane_was_live && !original.is_structured(),
                ))
                .context("attach restart generation overflow")?;
            resume_instance(storage, original, generation, outcome)?;
            crate::process::worker_registry::mark_restart_pending(
                session_id,
                quiesced.worker_generation,
            );
            Ok(())
        })();
        if let Err(error) = marker {
            warnings.push(format!("worker restart refused: {error:#}"));
        }
    }

    warnings
}

/// A TUI-initiated attach, handed to a background worker thread.
pub struct AttachProjectRequest {
    pub session_id: String,
    pub storage: Storage,
    pub instance: super::Instance,
    pub repo_path: PathBuf,
}

/// Result of [`perform_attach_project`], already phrased for the user.
pub struct AttachProjectResult {
    pub session_id: String,
    /// `Ok` carries the success notice, including any restart or container
    /// warning; `Err` carries the refusal or failure.
    pub outcome: Result<String, String>,
}

/// Everything about an attach that must not run on the TUI render thread.
pub fn perform_attach_project(request: AttachProjectRequest) -> AttachProjectResult {
    let session_id = request.session_id.clone();
    let outcome = attach_and_restart(request);
    AttachProjectResult {
        session_id,
        outcome,
    }
}

/// Plan, stop, convert, start again.
fn attach_and_restart(request: AttachProjectRequest) -> Result<String, String> {
    let storage = request.storage;
    let instance = &request.instance;

    let mut plan = plan(
        instance,
        storage.profile(),
        &request.repo_path,
        // The TUI picker has no place to confirm reusing a branch, so it takes the safe path and
        // refuses; `aoe session add-project --attach-existing-branch` is the way to opt in.
        ExistingBranch::Refuse,
    )
    .map_err(|e| format!("{e:#}"))?;

    let instance = reserve_attach(&storage, &mut plan).map_err(|e| format!("{e:#}"))?;
    let restarts = needs_restart(&plan, instance.is_sandboxed());
    let quiesced = if restarts {
        quiesce_for_conversion(&storage, &plan)
            .map_err(|error| format!("{:#}", cancel_attach(&plan, error)))?
    } else {
        Quiesced {
            attach_generation: instance.lifecycle_generation,
            ..Default::default()
        }
    };

    let outcome = match attach_planned(&storage, &request.session_id, &instance, plan) {
        Ok(outcome) => outcome,
        Err(e) => {
            // The session was stopped for an attach that then failed.
            resume_after_conversion(&storage, &instance, quiesced, None);
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
        message.push_str("\n\nRestarted the session so it comes up with the new repo.");
    } else {
        message.push_str(
            "\n\nThe agent is already working in this directory, so nothing was restarted.",
        );
    }
    for warning in resume_after_conversion(&storage, &instance, quiesced, Some(&outcome)) {
        message.push_str(&format!("\n\nWarning: {warning}"));
    }

    Ok(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{Instance, WorkspaceInfo, WorkspaceRepo, WorktreeInfo};

    #[cfg(unix)]
    fn stop_runtime_fixture(temp: &Path) -> (crate::session::test_support::EnvGuard, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let bin = temp.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let calls = temp.join("attach-runtime-calls");
        let live = temp.join("attach-tmux-live");
        std::fs::write(&live, b"initial pane").unwrap();
        let tmux = bin.join("tmux");
        std::fs::write(
            &tmux,
            format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> {calls}
while [ "$#" -gt 0 ]; do
    case "$1" in
        -S|-L) shift 2 ;;
        -u) shift ;;
        *) break ;;
    esac
done
case "$1" in
    has-session)
        test -f {live}
        exit $?
        ;;
    kill-session)
        rm -f -- {live}
        ;;
    new-session)
        launch=
        for arg in "$@"; do
            case "$arg" in
                'exec '*) launch="$arg"; break ;;
            esac
        done
        test -n "$launch" || exit 2
        /bin/sh -c "$launch" || exit $?
        : > {live}
        ;;
    display-message)
        test -f {live} || exit 1
        case "$*" in
            *'#{{pane_dead}}'*) printf '0\n' ;;
            *'#{{pane_current_command}}'*) printf 'claude\n' ;;
        esac
        ;;
esac
exit 0
"#,
                calls = crate::session::environment::shell_escape(&calls.to_string_lossy()),
                live = crate::session::environment::shell_escape(&live.to_string_lossy()),
            ),
        )
        .unwrap();
        std::fs::set_permissions(&tmux, std::fs::Permissions::from_mode(0o755)).unwrap();
        let agent = bin.join("claude");
        std::fs::write(
            &agent,
            format!(
                "#!/bin/sh\nprintf '%s\\n' agent-launch >> {}\n",
                crate::session::environment::shell_escape(&calls.to_string_lossy()),
            ),
        )
        .unwrap();
        std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755)).unwrap();
        (crate::session::test_support::path_prepended(&bin), calls)
    }

    fn attach_fixture(temp: &Path, profile: &str) -> (Storage, Instance, AttachPlan) {
        let primary = temp.join("src/primary");
        let added = temp.join("src/added");
        init_repo(&primary);
        init_repo(&added);
        let mut instance = Instance::new("attach admission", primary.to_str().unwrap());
        instance.source_profile = profile.to_string();
        let storage = Storage::open_unwatched(profile).unwrap();
        storage
            .update(|rows, _| {
                rows.push(instance.clone());
                Ok(())
            })
            .unwrap();
        let plan = plan(&instance, profile, &added, ExistingBranch::Refuse).unwrap();
        (storage, instance, plan)
    }

    #[test]
    #[serial_test::serial]
    fn attach_resolves_implicit_profile_before_original_storage_admission() {
        let temp = tempfile::tempdir().unwrap();
        let profile = "chosen-owner";
        let _home = isolated_profile(temp.path(), profile);
        crate::session::create_profile("alphabetically-first").unwrap();
        super::super::config::update_config(|config| {
            config.default_profile = profile.to_string();
        })
        .unwrap();
        let (storage, mut original, _) = attach_fixture(temp.path(), profile);
        original.status = super::super::Status::Starting;
        storage
            .update(|rows, _| {
                rows[0].status = original.status;
                Ok(())
            })
            .unwrap();
        let mut plan = plan(
            &original,
            "",
            &temp.path().join("src/added"),
            ExistingBranch::Refuse,
        )
        .unwrap();
        let admitted = reserve_attach(&storage, &mut plan).unwrap();
        let outcome = attach_planned(&storage, &original.id, &admitted, plan).unwrap();
        let row = storage
            .load_strict_for_worktree_ownership()
            .unwrap()
            .into_iter()
            .find(|row| row.id == original.id)
            .unwrap();
        assert_eq!(row.project_path, outcome.moved_to.unwrap());
        let added_checkout = Path::new(&outcome.repo.worktree_path);
        assert!(added_checkout.join("README.md").exists());
        let repository = git2::Repository::open(added_checkout).unwrap();
        let head = repository.head().unwrap();
        assert_eq!(head.shorthand().unwrap(), outcome.repo.branch);
        assert_eq!(row.source_profile, profile);
        assert!(Storage::open_unwatched("alphabetically-first")
            .unwrap()
            .load_strict_for_worktree_ownership()
            .unwrap()
            .is_empty());
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn attach_reservation_blocks_profile_rename_after_runtime_stop() {
        let temp = tempfile::tempdir().unwrap();
        let profile = "attach-stop-owner";
        let _home = isolated_profile(temp.path(), profile);
        crate::session::update_app_state(|state| {
            state.has_acknowledged_agent_hooks = true;
        })
        .unwrap();
        let (_path, calls) = stop_runtime_fixture(temp.path());
        let (storage, original, mut plan) = attach_fixture(temp.path(), profile);
        reserve_attach(&storage, &mut plan).unwrap();
        let destination = plan.workspace_dir.clone();
        let quiesced = quiesce_for_conversion(&storage, &plan).unwrap();
        assert!(quiesced.pane_was_live);
        assert!(!temp.path().join("attach-tmux-live").exists());
        let stopped_calls = std::fs::read(&calls).unwrap();
        assert!(String::from_utf8_lossy(&stopped_calls).contains("kill-session"));

        assert!(
            crate::session::rename_profile(profile, "attach-stop-original").is_err(),
            "an admitted attach must fence profile rename"
        );
        let _ = cancel_attach(&plan, anyhow::anyhow!("test cancellation"));
        let mut original = original;
        original.command = temp
            .path()
            .join("bin/claude")
            .to_string_lossy()
            .into_owned();
        storage
            .update(|rows, _| {
                rows.iter_mut()
                    .find(|row| row.id == original.id)
                    .unwrap()
                    .command
                    .clone_from(&original.command);
                Ok(())
            })
            .unwrap();
        let warnings = resume_after_conversion(&storage, &original, quiesced, None);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(temp.path().join("attach-tmux-live").exists());
        let resumed_calls = std::fs::read_to_string(&calls).unwrap();
        assert!(resumed_calls.contains("new-session"));
        assert!(resumed_calls.contains("agent-launch"));

        assert!(
            !destination.exists(),
            "cancelled attach performed checkout effects"
        );
        let durable = storage.load().unwrap().remove(0);
        assert_eq!(durable.id, original.id);
        assert!(durable.lifecycle_reservation.is_none());
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn attach_intent_reservation_does_not_expire_or_allow_checkout_adoption() {
        use crate::session::{LifecycleOperation, LifecycleReservationError};

        let temp = tempfile::tempdir().unwrap();
        let profile = "attach-stop-stale";
        let _home = isolated_profile(temp.path(), profile);
        let (storage, original, mut plan) = attach_fixture(temp.path(), profile);
        let admitted = reserve_attach(&storage, &mut plan).unwrap();

        let mut stale = admitted.clone();
        stale.lifecycle_reservation.as_mut().unwrap().at =
            Utc::now() - Instance::LIFECYCLE_RESERVATION_TTL - chrono::Duration::seconds(1);
        assert!(matches!(
            stale.try_acquire_lifecycle_reservation(
                LifecycleOperation::Launch,
                Instance::LIFECYCLE_RESERVATION_TTL,
                Utc::now(),
            ),
            Err(LifecycleReservationError::Busy(LifecycleOperation::Attach))
        ));

        let replacement = temp.path().join("peer-checkout");
        assert!(storage
            .update(|rows, _| {
                rows.iter_mut()
                    .find(|row| row.id == original.id)
                    .unwrap()
                    .project_path = replacement.to_string_lossy().into_owned();
                Ok(())
            })
            .is_err());
        assert!(!replacement.exists());

        let _ = cancel_attach(&plan, anyhow::anyhow!("test cancellation"));
        let durable = storage.load().unwrap().remove(0);
        assert_eq!(durable.project_path, original.project_path);
        assert!(durable.lifecycle_reservation.is_none());
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn attach_admission_refuses_launch_or_unreadable_inventory_before_any_stop() {
        for corrupt_inventory in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let profile = "attach-launch-admission";
            let _home = isolated_profile(temp.path(), profile);
            let (storage, _, mut plan) = attach_fixture(temp.path(), profile);
            if corrupt_inventory {
                crate::session::create_profile("attach-unreadable").unwrap();
                let foreign = Storage::open_unwatched("attach-unreadable").unwrap();
                std::fs::write(foreign.sessions_path(), b"{invalid inventory").unwrap();
            } else {
                storage
                    .update(|rows, _| {
                        rows[0].try_acquire_lifecycle_reservation(
                            crate::session::LifecycleOperation::Launch,
                            Instance::LIFECYCLE_RESERVATION_TTL,
                            Utc::now(),
                        )?;
                        Ok(())
                    })
                    .unwrap();
            }
            let _ = crate::tmux::fork_probe::take();
            let probe = crate::tmux::fork_probe::arm();
            assert!(reserve_attach(&storage, &mut plan).is_err());
            assert_eq!(
                crate::tmux::fork_probe::take(),
                0,
                "rejected admission reached tmux"
            );
            drop(probe);
            assert!(plan.reservation.is_none());
            let row = storage.load().unwrap().remove(0);
            if corrupt_inventory {
                assert!(row.lifecycle_reservation.is_none());
            } else {
                assert_eq!(
                    row.lifecycle_reservation.unwrap().op,
                    crate::session::LifecycleOperation::Launch
                );
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn attach_cancellation_preserves_unreadable_owner_rows() {
        let temp = tempfile::tempdir().unwrap();
        let profile = "attach-cancel-owner";
        let _home = isolated_profile(temp.path(), profile);
        let (storage, original, mut plan) = attach_fixture(temp.path(), profile);
        reserve_attach(&storage, &mut plan).unwrap();
        let mut rows = serde_json::to_value(storage.load().unwrap()).unwrap();
        let mut unreadable = rows[0].clone();
        unreadable["id"] = "unreadable-peer".into();
        unreadable["status"] = "invalid-status".into();
        rows.as_array_mut().unwrap().push(unreadable);
        let bytes = serde_json::to_vec(&rows).unwrap();
        std::fs::write(storage.sessions_path(), &bytes).unwrap();
        let _ = cancel_attach(&plan, anyhow::anyhow!("test cancellation"));
        assert_eq!(std::fs::read(storage.sessions_path()).unwrap(), bytes);
        assert!(Path::new(&original.project_path).is_dir());
    }

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
                base_branch: None,
                base_branch_override: None,
            }],
            created_at: Utc::now(),
            cleanup_on_delete: true,
        });
        inst
    }

    #[test]
    fn plan_refuses_states_that_are_never_attachable() {
        use super::super::Status;
        type Setup = fn(&mut Instance);
        // Each refusal must win over the not-a-git-repo error; a Running session reaches it.
        let cases: [(Setup, &str); 6] = [
            (|i| i.scratch = true, "scratch session"),
            (
                |i| i.status = Status::Creating,
                "being created or is being deleted",
            ),
            (
                |i| i.status = Status::Deleting,
                "being created or is being deleted",
            ),
            (|i| i.trashed_at = Some(Utc::now()), "in the trash"),
            (|i| i.archived_at = Some(Utc::now()), "archived"),
            (|i| i.status = Status::Running, "not a git repository"),
        ];
        for (setup, want) in cases {
            let mut inst = Instance::new("Attach", "/tmp/attach");
            setup(&mut inst);
            let Err(err) = plan(
                &inst,
                "default",
                Path::new("/tmp/definitely-not-a-repo"),
                ExistingBranch::Refuse,
            ) else {
                panic!("{want}: must be refused");
            };
            let msg = format!("{err:#}");
            assert!(msg.contains(want), "{want}: {msg}");
        }
    }

    #[test]
    #[serial_test::serial]
    fn sandbox_release_is_a_no_op_without_a_sandbox() {
        assert!(
            !crate::session::worktree_edit::ensure_sandbox_container_released(
                "no-such-session",
                false,
            )
        );
    }

    fn execute_fixture(instance: &Instance, mut plan: AttachPlan) -> Result<AttachOutcome> {
        let storage = Storage::new_unwatched(&plan.profile)?;
        storage.update(|rows, _| {
            rows.push(instance.clone());
            Ok(())
        })?;
        let admitted = reserve_attach(&storage, &mut plan)?;
        attach_planned(&storage, &instance.id, &admitted, plan)
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

    fn init_repo(path: &Path) {
        std::fs::create_dir_all(path).expect("create repo dir");
        git_in(path, &["init", "-q"]);
        git_in(path, &["config", "user.email", "test@example.com"]);
        git_in(path, &["config", "user.name", "Test"]);
        std::fs::write(path.join("README.md"), "x").expect("seed file");
        git_in(path, &["add", "."]);
        git_in(path, &["commit", "-qm", "init"]);
    }

    fn isolated_profile(temp: &Path, name: &str) -> crate::session::test_support::AppDirGuard {
        let guard = crate::session::test_support::isolate_app_dir_at(temp);
        crate::session::create_profile(name).expect("profile");
        guard
    }

    #[test]
    #[serial_test::serial]
    fn attach_admission_rejects_a_checkout_changed_after_planning() {
        let temp = tempfile::tempdir().unwrap();
        let profile = "attach-checkout-refresh";
        let _home = isolated_profile(temp.path(), profile);
        let backend = temp.path().join("src/backend");
        let frontend = temp.path().join("src/frontend");
        init_repo(&backend);
        init_repo(&frontend);
        let worktree = temp.path().join("src/backend-hook");
        git_in(
            &backend,
            &[
                "worktree",
                "add",
                "-b",
                "feature/hook",
                worktree.to_str().unwrap(),
            ],
        );
        let mut original = Instance::new("hook", worktree.to_str().unwrap());
        original.source_profile = profile.to_string();
        original.worktree_info = Some(WorktreeInfo {
            branch: "feature/hook".into(),
            main_repo_path: backend.to_string_lossy().into_owned(),
            managed_by_aoe: true,
            created_at: Utc::now(),
            base_branch: None,
        });
        let storage = Storage::open_unwatched(profile).unwrap();
        storage
            .update(|rows, _| {
                rows.push(original.clone());
                Ok(())
            })
            .unwrap();
        let mut attach_plan = plan(&original, profile, &frontend, ExistingBranch::Refuse).unwrap();
        let destination = attach_plan.workspace_dir.clone();
        let moved = temp.path().join("src/backend-moved");
        git_in(
            &backend,
            &[
                "worktree",
                "move",
                worktree.to_str().unwrap(),
                moved.to_str().unwrap(),
            ],
        );
        storage
            .update(|rows, _| {
                rows[0].project_path = moved.to_string_lossy().into_owned();
                Ok(())
            })
            .unwrap();

        let error = reserve_attach(&storage, &mut attach_plan).unwrap_err();
        assert!(error.to_string().contains("checkout changed"), "{error:#}");
        assert!(!destination.exists(), "stale plan performed Git work");
        assert!(moved.exists());
        assert!(git2::Repository::open(&frontend)
            .unwrap()
            .find_branch("feature/hook", git2::BranchType::Local)
            .is_err());
    }

    #[test]
    #[serial_test::serial]
    fn attach_keeps_resources_when_purge_owns_row_or_inventory_is_unreadable() {
        use super::super::deletion::{DeletionRequest, PurgeReservation, PurgeTransaction};
        for active_purge in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let profile = "attach-conflict";
            let _home = isolated_profile(temp.path(), profile);
            let backend = temp.path().join("src/backend");
            let frontend = temp.path().join("src/frontend");
            init_repo(&backend);
            init_repo(&frontend);
            let worktree = temp.path().join("src/backend-conflict");
            git_in(
                &backend,
                &[
                    "worktree",
                    "add",
                    "-b",
                    "feature/conflict",
                    worktree.to_str().unwrap(),
                ],
            );
            let mut original = Instance::new("conflict", worktree.to_str().unwrap());
            original.source_profile = profile.into();
            original.worktree_info = Some(WorktreeInfo {
                branch: "feature/conflict".into(),
                main_repo_path: backend.to_string_lossy().into_owned(),
                managed_by_aoe: true,
                created_at: Utc::now(),
                base_branch: None,
            });
            let storage = Storage::open_unwatched(profile).unwrap();
            storage
                .update(|rows, _| {
                    rows.push(original.clone());
                    Ok(())
                })
                .unwrap();
            let mut attach_plan =
                plan(&original, profile, &frontend, ExistingBranch::Refuse).unwrap();
            let destination = attach_plan.workspace_dir.clone();
            let purge = if active_purge {
                match PurgeTransaction::reserve(
                    storage.clone(),
                    DeletionRequest {
                        session_id: original.id.clone(),
                        instance: original.clone(),
                        delete_worktree: true,
                        delete_branch: true,
                        delete_sandbox: false,
                        force_delete: false,
                        detach_hooks: true,
                        keep_scratch: false,
                    },
                )
                .unwrap()
                {
                    PurgeReservation::Reserved(transaction) => Some(transaction.run_hooks()),
                    PurgeReservation::Rejected(_) => panic!("purge rejected"),
                }
            } else {
                let other = Storage::new_unwatched("unreadable").unwrap();
                other.update(|_, _| Ok(())).unwrap();
                std::fs::write(other.sessions_path(), "not json").unwrap();
                None
            };
            assert!(reserve_attach(&storage, &mut attach_plan).is_err());
            assert!(worktree.exists());
            assert!(!destination.exists());
            let rows = storage.load().unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].project_path, original.project_path);
            assert!(rows[0].workspace_info.is_none());
            assert!(git2::Repository::open(&frontend)
                .unwrap()
                .find_branch("feature/conflict", git2::BranchType::Local)
                .is_err());
            if let Some(purge) = purge {
                let result = purge.complete();
                assert!(result.success, "{:?}", result.errors);
                assert!(!worktree.exists());
                assert!(storage.load().unwrap().is_empty());
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn attach_reservation_blocks_owner_adoption_and_profile_mutation() {
        for change in ["identity", "adoption", "profile"] {
            let temp = tempfile::tempdir().unwrap();
            let _home = isolated_profile(temp.path(), "owner");
            let (storage, original, mut plan) = attach_fixture(temp.path(), "owner");
            reserve_attach(&storage, &mut plan).unwrap();
            let target = plan.added_worktree.clone();
            let added_repo = plan.added_main_repo.clone();
            let branch = plan.added_branch.branch.clone();

            let refused = match change {
                "identity" => storage.update(|rows, _| {
                    rows.iter_mut()
                        .find(|row| row.id == original.id)
                        .unwrap()
                        .created_at += chrono::Duration::seconds(1);
                    Ok(())
                }),
                "adoption" => {
                    let peer = Storage::new_unwatched("peer").unwrap();
                    peer.update(|rows, _| {
                        rows.push(Instance::new("adopter", target.to_str().unwrap()));
                        Ok(())
                    })
                }
                "profile" => crate::session::rename_profile("owner", "renamed"),
                _ => unreachable!(),
            };
            assert!(
                refused.is_err(),
                "{change}: reserved resources were adopted"
            );

            let _ = cancel_attach(&plan, anyhow::anyhow!("test cancellation"));
            assert!(Path::new(&original.project_path).exists());
            assert!(
                !target.exists(),
                "{change}: rejected mutation performed Git work"
            );
            assert!(!GitWorktree::new(added_repo)
                .unwrap()
                .branch_exists(&branch)
                .unwrap());
            assert!(super::super::move_journal::scan().entries.is_empty());
        }
    }

    #[test]
    #[serial_test::serial]
    fn appending_to_an_existing_workspace_moves_nothing() {
        for case in ["created", "reused"] {
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
                    base_branch: None,
                    base_branch_override: None,
                }],
                created_at: Utc::now(),
                cleanup_on_delete: true,
            });

            if case == "reused" {
                git_in(&frontend, &["branch", "featx"]);
            }
            let plan = plan(
                &inst,
                "attach-append",
                &frontend,
                if case == "reused" {
                    ExistingBranch::Attach
                } else {
                    ExistingBranch::Refuse
                },
            )
            .expect("attaching to a workspace session must be accepted");
            assert!(
                !plan.moves_session,
                "an existing workspace is not moved, so nothing has to be stopped"
            );
            assert_eq!(plan.workspace_dir(), workspace);

            let outcome = execute_fixture(&inst, plan).expect("the worktree must be created");
            assert!(outcome.moved_to.is_none());
            assert!(workspace.join("frontend/.git").exists());
            assert!(backend_wt.join(".git").exists());
            assert_eq!(
                outcome
                    .workspace_info
                    .repos
                    .iter()
                    .map(|repo| repo.name.as_str())
                    .collect::<Vec<_>>(),
                vec!["backend", "frontend"],
            );
            let durable = Storage::new_unwatched("attach-append")
                .unwrap()
                .load()
                .unwrap()
                .remove(0);
            assert_eq!(durable.project_path, workspace.to_string_lossy());
            assert!(durable.lifecycle_reservation.is_none());
            assert!(super::super::move_journal::scan().entries.is_empty());
        }
    }

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

        let blocker = plan.workspace_dir().join("frontend");
        std::fs::create_dir_all(&blocker).unwrap();
        std::fs::write(blocker.join("in-the-way.txt"), "x").unwrap();

        let Err(err) = execute_fixture(&inst, plan) else {
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
    #[serial_test::serial]
    fn converting_a_session_keeps_its_diff_base() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = isolated_profile(temp.path(), "attach-diff-base");

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

        let mut inst = Instance::new("Worktree Session", session_wt.to_str().unwrap());
        inst.worktree_info = Some(WorktreeInfo {
            branch: "featx".to_string(),
            main_repo_path: backend.to_string_lossy().to_string(),
            managed_by_aoe: true,
            created_at: Utc::now(),
            base_branch: Some("develop".to_string()),
        });
        inst.base_branch_override = Some("upstream/main".to_string());

        let plan = plan(&inst, "attach-diff-base", &frontend, ExistingBranch::Refuse)
            .expect("the attach itself is valid");
        let prepared = execute_fixture(&inst, plan).expect("the worktree must be created");

        let primary = prepared
            .workspace_info
            .repos
            .iter()
            .find(|r| r.name == "backend")
            .expect("the session's own repo becomes a workspace member");
        assert_eq!(primary.base_branch.as_deref(), Some("develop"));
        assert_eq!(
            primary.base_branch_override.as_deref(),
            Some("upstream/main"),
            "the override the user picked must survive the conversion"
        );

        let added = prepared
            .workspace_info
            .repos
            .iter()
            .find(|r| r.name == "frontend")
            .expect("the attached repo is recorded");
        assert_eq!(added.base_branch_override, None);
    }

    #[test]
    fn repo_leaf_name_uses_the_main_repo_directory() {
        assert_eq!(repo_leaf_name(Path::new("/tmp/src/frontend")), "frontend");
        assert_eq!(repo_leaf_name(Path::new("/")), "repo");
    }

    #[test]
    fn reject_duplicate_cases() {
        let workspace = workspace_instance();
        let mut own = Instance::new("WT", "/tmp/worktrees/feature");
        own.worktree_info = Some(WorktreeInfo {
            branch: "feature/abc".to_string(),
            main_repo_path: "/tmp/src/backend".to_string(),
            managed_by_aoe: true,
            created_at: Utc::now(),
            base_branch: None,
        });
        // (session, repo, leaf, expected error fragment; None = accepted)
        let cases = [
            (
                &workspace,
                "/tmp/src/backend",
                "backend-alias",
                Some("already attached"),
            ),
            (
                &workspace,
                "/other/src/BackEnd",
                "BackEnd",
                Some("collide on disk"),
            ),
            (
                &own,
                "/tmp/src/backend",
                "backend",
                Some("already this session's own repo"),
            ),
            (&workspace, "/tmp/src/frontend", "frontend", None),
        ];
        for (inst, repo, leaf, want) in cases {
            let got = reject_duplicate(inst, Path::new(repo), leaf)
                .err()
                .map(|e| e.to_string());
            match want {
                Some(want) => assert!(
                    got.as_deref().is_some_and(|e| e.contains(want)),
                    "{repo} {leaf}: {got:?}"
                ),
                None => assert_eq!(got, None, "{repo} {leaf}"),
            }
        }
    }

    #[test]
    fn plain_session_branch_comes_from_the_title() {
        assert_eq!(
            branch_for_plain_session("Fix the auth bug"),
            "fix-the-auth-bug"
        );
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

        assert_eq!(session_branch(&Instance::new("Plain", "/tmp/plain")), None);
    }
}
