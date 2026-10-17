//! Canonical rename admission keeps namespace authority while Git and Docker run unlocked.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::Utc;

use super::claim::ResourceReservation;
use super::move_journal::{
    CallbackCheckout, EffectObservation, IntentOwner, ProfileAuthority, ResourceEffect,
    ResourceIntent,
};
use super::storage::{self, OwnershipGuard, StorageFlock};
use super::{Instance, LifecycleOperation, Storage};

#[cfg(test)]
type AfterEffectsHook = Box<dyn FnOnce(&OwnershipGuard, &Instance, &Instance)>;
#[cfg(test)]
thread_local! {
    static AFTER_EFFECTS: std::cell::RefCell<Option<AfterEffectsHook>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn with_after_effects<T>(
    hook: impl FnOnce(&OwnershipGuard, &Instance, &Instance) + 'static,
    run: impl FnOnce() -> T,
) -> T {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            AFTER_EFFECTS.with(|slot| slot.borrow_mut().take());
        }
    }
    AFTER_EFFECTS.with(|slot| assert!(slot.borrow_mut().replace(Box::new(hook)).is_none()));
    let _reset = Reset;
    run()
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum RenameRejected {
    #[error("Cannot edit the workdir name or branch while the session is active; stop it first")]
    Active,
    #[error("Session not found")]
    Missing,
    #[error("Session does not use a worktree")]
    NotWorktree,
    #[error("Renaming is unified while session.tie_workdir_to_name is on; use 'aoe session rename --title <name>' instead")]
    Tied,
    #[error("Could not stop the structured view worker before renaming; retry in a moment")]
    WorkerShutdown,
    #[error(transparent)]
    Duplicate(anyhow::Error),
}

pub(crate) enum Edit {
    Session {
        title: Option<String>,
        group: Option<String>,
        profile: Option<String>,
        rename_branch: bool,
        branch: Option<String>,
    },
    Workdir {
        name: String,
        rename_branch: bool,
    },
}

pub(crate) struct Request {
    pub baseline: Instance,
    pub edit: Edit,
    pub file_watch: Arc<crate::file_watch::FileWatchService>,
}

pub(crate) struct Committed {
    pub baseline: Instance,
    pub original: Instance,
    pub instance: Instance,
    pub warning: Option<String>,
    pub storage: Storage,
}

struct Guards {
    root: OwnershipGuard,
    _identity: StorageFlock,
    _title: StorageFlock,
    _lifecycle: StorageFlock,
}

fn lock(storage: &Storage, id: &str) -> Result<Guards> {
    let root = storage::acquire_ownership_lock()?;
    let identity = storage::acquire_session_identity_lock_with_ownership(&root)?;
    let title = storage::acquire_session_title_lock_with_ownership(&root, id)?;
    let lifecycle = storage.acquire_instance_lifecycle_lock_with_ownership(&root, id)?;
    Ok(Guards {
        root,
        _identity: identity,
        _title: title,
        _lifecycle: lifecycle,
    })
}

fn duplicate(rows: &[Instance], candidate: &Instance, exclude: Option<&str>) -> Result<()> {
    if super::is_duplicate_session(
        rows.iter(),
        &candidate.title,
        &candidate.project_path,
        exclude,
    ) {
        return Err(
            RenameRejected::Duplicate(super::duplicate_session_error(&candidate.title)).into(),
        );
    }
    Ok(())
}

fn rename_failure(
    transaction: &ResourceReservation,
    error: anyhow::Error,
    compensate: bool,
) -> anyhow::Error {
    let cleanup = if compensate {
        match transaction.rollback() {
            Ok(()) => {
                return error.context("Rename effects were rolled back and its reservation retired")
            }
            Err(cleanup) => Some(format!("compensation could not finish: {cleanup:#}")),
        }
    } else {
        None
    };
    let path = &transaction.journal().path;
    match path.try_exists() {
        Ok(false) => {
            return error.context(cleanup.unwrap_or_else(|| "No intent file remains".to_owned()))
        }
        Err(check) => {
            return error.context(format!(
                "Could not inspect rename intent {}: {check:#}",
                path.display()
            ))
        }
        Ok(true) => {}
    }
    let reason = cleanup.unwrap_or_else(|| format!("{error:#}"));
    let recorded = storage::acquire_ownership_lock()
        .and_then(|root| transaction.note_conflict_with_ownership(&root, reason));
    match recorded {
        Ok(()) => error.context(format!(
            "Rename intent retained at {}. Completed Git effects can still roll forward during recovery; inspect and reload the session before retrying",
            path.display()
        )),
        Err(recording) => error.context(format!(
            "Rename intent at {} could not record its diagnostic: {recording:#}",
            path.display()
        )),
    }
}

/// A one-shot terminal observation must not inherit Stopped's poll short circuit
/// or Error's display cooldown. Unknown tmux ownership never proves quiescence.
fn observe_terminal_preflight(original: &Instance) -> Result<Instance> {
    super::config::profile_config::resolve_config(&original.source_profile)?;
    let refresh = crate::tmux::refresh_session_cache();
    let session = original.tmux_session()?;
    let existence = crate::tmux::resolved_agent_existence(&original.id, &session, refresh);
    anyhow::ensure!(
        existence != crate::tmux::SessionExistence::Unknown,
        "Could not determine whether the terminal session is running"
    );
    let mut observed = original.clone();
    observed.last_error_check = None;
    observed.last_start_time = None;
    if existence == crate::tmux::SessionExistence::Absent {
        observed.status = super::Status::Stopped;
        observed.idle_entered_at = None;
        observed.last_error = None;
    }
    if existence == crate::tmux::SessionExistence::Present {
        observed.archived_at = None;
        if observed.status == super::Status::Stopped {
            observed.status = super::Status::Idle;
        }
    }
    observed.update_status_once(None, Some(session.name()));
    if observed.status.blocks_worktree_edit() {
        return Err(RenameRejected::Active.into());
    }
    Ok(observed)
}

pub(crate) fn perform(
    mut request: Request,
    preflight: impl FnOnce(&Instance, bool) -> Result<()>,
) -> Result<Committed> {
    let storage = Storage::open(
        &request.baseline.source_profile,
        Arc::clone(&request.file_watch),
    )?;
    let rows = storage.load_strict_for_worktree_ownership()?;
    let mut original = rows
        .iter()
        .find(|row| row.id == request.baseline.id)
        .cloned()
        .ok_or(RenameRejected::Missing)?;
    anyhow::ensure!(
        original.created_at == request.baseline.created_at,
        "Session incarnation changed before rename"
    );
    if original.has_fresh_lifecycle_reservation(Utc::now()) {
        return Err(super::LifecycleReservationError::Busy(
            original.lifecycle_reservation.as_ref().unwrap().op,
        )
        .into());
    }
    anyhow::ensure!(!original.is_trashed(), "Cannot rename a trashed session");
    request
        .baseline
        .source_profile
        .clone_from(&original.source_profile);
    let discovered_baseline = original.clone();
    let mut reconcile_cache = super::worktree_reconcile::ReconcileCache::default();
    let mut candidate = original.clone();
    let mut name = None;
    let mut branch_only = None;
    let mut rename_branch = false;
    let mut target_profile = None;
    let standalone = matches!(&request.edit, Edit::Workdir { .. });
    match request.edit {
        Edit::Session {
            title,
            group,
            profile,
            rename_branch: rename,
            branch,
        } => {
            let title_requested = title.is_some();
            if let Some(title) = title {
                candidate.title = title;
            }
            if let Some(group) = group {
                candidate.group_path = group;
            }
            target_profile = profile.filter(|profile| *profile != original.source_profile);
            let tied = original.tie_workdir_applies(
                super::config::profile_config::resolve_config_or_warn(&original.source_profile)
                    .session
                    .tie_workdir_to_name,
            );
            if let Some(branch) = branch {
                branch_only = Some(branch);
            } else if tied && (title_requested || rename) {
                name = Some(super::worktree_edit::worktree_leaf_from_title(
                    &candidate.title,
                ));
                rename_branch = rename;
            } else {
                anyhow::ensure!(!rename || tied, "--rename-branch only applies to a tied aoe-managed worktree session (session.tie_workdir_to_name)");
            }
        }
        Edit::Workdir {
            name: requested,
            rename_branch: rename,
        } => {
            if original.worktree_info.is_none() {
                return Err(RenameRejected::NotWorktree.into());
            }
            if original.tie_workdir_applies(
                super::config::profile_config::resolve_config_or_warn(&original.source_profile)
                    .session
                    .tie_workdir_to_name,
            ) {
                return Err(RenameRejected::Tied.into());
            }
            name = Some(requested);
            rename_branch = rename;
        }
    }
    let target = target_profile
        .as_ref()
        .map(|profile| Storage::open(profile, Arc::clone(&request.file_watch)))
        .transpose()?;
    if let Some(name) = &name {
        candidate.project_path =
            super::worktree_edit::target_worktree_path(Path::new(&original.project_path), name)
                .ok_or_else(|| {
                    super::worktree_edit::WorktreeEditError::NoParent(
                        original.project_path.clone().into(),
                    )
                })?
                .to_string_lossy()
                .into_owned();
        if rename_branch {
            if let Some(info) = candidate.worktree_info.as_mut() {
                info.branch = super::builder::git_sanitize_branch_name(name);
            }
        }
    }
    if let Some(branch) = &branch_only {
        candidate
            .worktree_info
            .as_mut()
            .context("Branch-only rename requires a managed worktree")?
            .branch
            .clone_from(branch);
    }
    let mut moves = original.project_path != candidate.project_path;
    let changes_branch = original.worktree_info.as_ref().map(|info| &info.branch)
        != candidate.worktree_info.as_ref().map(|info| &info.branch);
    let mut edits_git = moves || changes_branch;
    if standalone && !edits_git {
        return Err(super::worktree_edit::WorktreeEditError::Unchanged.into());
    }
    if let Some(target) = &target {
        let target_rows = target.load_strict_for_worktree_ownership()?;
        anyhow::ensure!(
            !target_rows.iter().any(|row| row.id == original.id),
            "Session already exists in target profile"
        );
        duplicate(&target_rows, &candidate, None)?;
    } else {
        duplicate(&rows, &candidate, Some(&original.id))?;
    }
    if edits_git
        && branch_only.is_none()
        && (matches!(
            original.status,
            super::Status::Creating | super::Status::Deleting
        ) || (original.is_structured()
            && (original.status.blocks_worktree_edit()
                || request.baseline.status.blocks_worktree_edit())))
    {
        return Err(RenameRejected::Active.into());
    }
    if edits_git {
        super::worktree_reconcile::observe_without_lease(&mut original, &mut reconcile_cache)?;
        if let Some(name) = &name {
            candidate.project_path =
                super::worktree_edit::target_worktree_path(Path::new(&original.project_path), name)
                    .ok_or_else(|| {
                        super::worktree_edit::WorktreeEditError::NoParent(
                            original.project_path.clone().into(),
                        )
                    })?
                    .to_string_lossy()
                    .into_owned();
        } else {
            candidate.project_path.clone_from(&original.project_path);
        }
        moves = original.project_path != candidate.project_path;
        edits_git = moves || changes_branch;
        if standalone && !edits_git {
            return Err(super::worktree_edit::WorktreeEditError::Unchanged.into());
        }
    }
    let requires_quiescence = edits_git && branch_only.is_none();
    if edits_git {
        if let Some(info) = &original.worktree_info {
            reconcile_cache.entries(&info.main_repo_path)?;
        }
    }
    if let Some(branch) = &branch_only {
        let _ = super::worktree_edit::prepare_worktree_branch_rename(
            original
                .worktree_info
                .as_ref()
                .context("Branch-only rename requires a managed worktree")?,
            Path::new(&original.project_path),
            branch,
        )?;
    }
    if let Some(name) = name.as_ref().filter(|_| edits_git) {
        let info = original
            .worktree_info
            .as_ref()
            .ok_or(RenameRejected::NotWorktree)?;
        if !info.managed_by_aoe {
            return Err(super::worktree_edit::WorktreeEditError::NotManaged.into());
        }
        if name.trim().is_empty() {
            return Err(super::worktree_edit::WorktreeEditError::EmptyName.into());
        }
        if !Path::new(&original.project_path).try_exists()? {
            return Err(super::worktree_edit::WorktreeEditError::SourceMissing(
                original.project_path.clone().into(),
            )
            .into());
        }
        if moves && Path::new(&candidate.project_path).try_exists()? {
            return Err(super::worktree_edit::WorktreeEditError::TargetExists(
                candidate.project_path.clone().into(),
            )
            .into());
        }
        if changes_branch {
            let branch = &candidate
                .worktree_info
                .as_ref()
                .ok_or(RenameRejected::NotWorktree)?
                .branch;
            super::worktree_edit::prepare_worktree_branch_rename(
                info,
                Path::new(&original.project_path),
                branch,
            )?;
        }
    }
    let source_checkouts = if changes_branch {
        let info = original
            .worktree_info
            .as_ref()
            .ok_or(RenameRejected::NotWorktree)?;
        crate::git::GitWorktree::new(info.main_repo_path.clone().into())?
            .list_worktrees()?
            .into_iter()
            .filter(|checkout| checkout.branch.as_deref() == Some(info.branch.as_str()))
            .map(|checkout| checkout.path)
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    drop(rows);
    let guards = lock(&storage, &request.baseline.id)?;
    let fresh_rows = storage.load_strict_for_worktree_ownership()?;
    let mut fresh = fresh_rows
        .iter()
        .find(|row| row.id == original.id)
        .cloned()
        .ok_or(RenameRejected::Missing)?;
    anyhow::ensure!(
        super::move_journal::row_resources_match(&fresh, &discovered_baseline)
            && fresh.lifecycle_reservation == discovered_baseline.lifecycle_reservation,
        "Session checkout changed during rename discovery"
    );
    anyhow::ensure!(
        !fresh.has_fresh_lifecycle_reservation(Utc::now()),
        "Session has a lifecycle operation in progress"
    );
    if edits_git {
        super::worktree_reconcile::reconcile_and_persist_with_ownership(
            &storage,
            &guards.root,
            &mut fresh,
            &mut reconcile_cache,
        )?;
    }
    anyhow::ensure!(
        super::attach_project::same_checkout(&original, &fresh),
        "Session checkout reconciliation changed during rename discovery"
    );
    let planned = candidate;
    candidate = fresh.clone();
    if planned.title != original.title {
        candidate.title = planned.title;
    }
    if planned.group_path != original.group_path {
        candidate.group_path = planned.group_path;
    }
    candidate.project_path = planned.project_path;
    candidate.worktree_info = planned.worktree_info;
    original = fresh;
    if let Some(target) = &target {
        let target_rows = target.load_strict_for_worktree_ownership()?;
        anyhow::ensure!(
            !target_rows.iter().any(|row| row.id == original.id),
            "Session already exists in target profile"
        );
        duplicate(&target_rows, &candidate, None)?;
    } else {
        duplicate(&fresh_rows, &candidate, Some(&original.id))?;
    }
    if edits_git {
        let info = original
            .worktree_info
            .as_ref()
            .ok_or(RenameRejected::NotWorktree)?;
        super::worktree_edit::ensure_worktree_unshared(
            &guards.root,
            &original.source_profile,
            &original.id,
            Path::new(&original.project_path),
            info,
            moves.then_some(Path::new(&candidate.project_path)),
            changes_branch,
        )?;
        let git = crate::git::GitWorktree::new(info.main_repo_path.clone().into())?;
        if changes_branch {
            let branch = &candidate
                .worktree_info
                .as_ref()
                .ok_or(RenameRejected::NotWorktree)?
                .branch;
            super::worktree_edit::revalidate_worktree_branch_rename(
                info,
                Path::new(&original.project_path),
                branch,
            )?;
            let live = git.list_worktrees()?;
            anyhow::ensure!(
                source_checkouts
                    .iter()
                    .all(|path| live.iter().any(|checkout| checkout.path == *path
                        && checkout.branch.as_deref() == Some(info.branch.as_str())))
                    && live
                        .iter()
                        .filter(|checkout| checkout.branch.as_deref() == Some(info.branch.as_str()))
                        .count()
                        == source_checkouts.len(),
                "Source branch checkouts changed during rename discovery"
            );
        }
        if moves {
            anyhow::ensure!(
                Path::new(&original.project_path).try_exists()?,
                "Worktree source disappeared during rename discovery"
            );
            if Path::new(&candidate.project_path).try_exists()? {
                return Err(super::worktree_edit::WorktreeEditError::TargetExists(
                    candidate.project_path.clone().into(),
                )
                .into());
            }
        }
    }
    let mut reserved = original.clone();
    let generation = reserved.try_acquire_lifecycle_reservation(
        LifecycleOperation::Rename,
        Instance::LIFECYCLE_RESERVATION_TTL,
        Utc::now(),
    )?;
    candidate.lifecycle_generation = generation;
    candidate.lifecycle_reservation = None;
    let mut effects = vec![ResourceEffect::Callback {
        name: "rename-preflight".to_owned(),
        directory: original.project_path.clone().into(),
        checkouts: original
            .worktree_info
            .iter()
            .filter(|_| edits_git)
            .map(|info| CallbackCheckout {
                repository: info.main_repo_path.clone().into(),
                path: original.project_path.clone().into(),
                branch: Some(info.branch.clone()),
            })
            .collect(),
    }];
    if moves && original.is_sandboxed() {
        effects.push(ResourceEffect::ReleaseContainer);
    }
    if changes_branch {
        let info = original
            .worktree_info
            .as_ref()
            .ok_or(RenameRejected::NotWorktree)?;
        effects.push(ResourceEffect::RenameBranch {
            repository: info.main_repo_path.clone().into(),
            old: info.branch.clone(),
            new: candidate
                .worktree_info
                .as_ref()
                .context("requested branch metadata missing")?
                .branch
                .clone(),
            checkouts: source_checkouts,
        });
    }
    if moves {
        let info = original
            .worktree_info
            .as_ref()
            .ok_or(RenameRejected::NotWorktree)?;
        effects.push(ResourceEffect::MoveWorktree {
            repository: info.main_repo_path.clone().into(),
            from: original.project_path.clone().into(),
            to: candidate.project_path.clone().into(),
        });
    }
    let count = effects.len();
    let mut intent = ResourceIntent::new(
        IntentOwner {
            profile: ProfileAuthority::capture(&storage)?,
            id: original.id.clone(),
            created_at: original.created_at,
            generation,
        },
        ProfileAuthority::capture(target.as_ref().unwrap_or(&storage))?,
        LifecycleOperation::Rename,
        Some(original.clone()),
        Some(candidate),
        effects,
        Vec::new(),
    )?;
    intent.quiesced = !requires_quiescence && !original.status.blocks_worktree_edit();
    let admission =
        ResourceReservation::admit_with_ownership(&storage, &guards.root, intent, reserved)?;
    original = admission.instance;
    let transaction = admission.transaction;
    drop(guards);

    let observed = match transaction.run_effect(0, || {
        preflight(&original, moves)?;
        let observed = if requires_quiescence && !original.is_structured() {
            observe_terminal_preflight(&original)?
        } else {
            original.clone()
        };
        Ok((observed, None))
    }) {
        Ok(Ok(observed)) => observed,
        Ok(Err(error)) => return Err(rename_failure(&transaction, error, true)),
        Err(error) => return Err(rename_failure(&transaction, error, false)),
    };
    if requires_quiescence {
        let recorded = (|| -> Result<()> {
            let guards = lock(&storage, &original.id)?;
            transaction.record_rename_preflight_with_ownership(&guards.root, &observed)
        })();
        if let Err(error) = recorded {
            return Err(rename_failure(&transaction, error, true));
        }
    }
    for index in 1..count {
        let effect = transaction.effect(index)?;
        let result = transaction.run_effect(index, || {
            if matches!(effect, ResourceEffect::ReleaseContainer) {
                let held =
                    super::worktree_edit::ensure_sandbox_container_released(&original.id, true);
                Ok((
                    held,
                    Some(EffectObservation {
                        container_released: Some(!held),
                        ..EffectObservation::default()
                    }),
                ))
            } else {
                effect.execute().map(|_| (false, None))
            }
        });
        match result {
            Ok(Ok(false)) => {}
            Ok(Ok(true)) => return Err(rename_failure(&transaction,
                anyhow::anyhow!("Stop the session before renaming: its sandbox container still holds the worktree"), true)),
            Ok(Err(error)) => return Err(rename_failure(&transaction, error, true)),
            Err(error) => return Err(rename_failure(&transaction, error, false)),
        }
    }
    let execution = (|| -> Result<Instance> {
        let guards = lock(&storage, &original.id)?;
        #[cfg(test)]
        if let Some(hook) = AFTER_EFFECTS.with(|slot| slot.borrow_mut().take()) {
            let entry = transaction.journal().read()?;
            let super::move_journal::MoveAuthority::Resource { intent } = &entry.authority else {
                unreachable!()
            };
            hook(
                &guards.root,
                &original,
                intent
                    .after
                    .as_ref()
                    .context("rename has no requested row")?,
            );
        }
        if requires_quiescence {
            let current = storage
                .load_strict_for_worktree_ownership()?
                .into_iter()
                .find(|row| row.id == original.id && row.created_at == original.created_at)
                .context("original session disappeared before rename publication")?;
            if current.status.blocks_worktree_edit() {
                return Err(RenameRejected::Active.into());
            }
        }
        transaction
            .publish_with_ownership(&guards.root, |rows, candidate| {
                duplicate(rows, candidate, Some(&original.id))
            })?
            .context("rename publication returned no row")
    })();
    let candidate = match execution {
        Ok(candidate) => candidate,
        Err(error) => {
            let compensate = error.downcast_ref::<RenameRejected>().is_some();
            return Err(rename_failure(&transaction, error, compensate));
        }
    };
    let warning = if original.title != candidate.title && !candidate.is_structured() {
        crate::tmux::rekey_session(&candidate.id, &original.title, &candidate.title).err()
            .map(|error| format!("Session metadata was renamed, but its live tmux session could not be rekeyed: {error}"))
    } else {
        None
    };
    Ok(Committed {
        baseline: request.baseline,
        original,
        instance: candidate,
        warning,
        storage: target.unwrap_or(storage),
    })
}

#[cfg(test)]
mod tests;
