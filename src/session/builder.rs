//! Instance creation and canonical filesystem publication.

use std::{collections::HashSet, path::PathBuf};

use anyhow::{bail, Context, Result};
use chrono::Utc;

use crate::containers;
use crate::git::error::GitError;
use crate::git::GitWorktree;

use super::{
    civilizations, Config, Instance, SandboxInfo, WorkspaceInfo, WorkspaceRepo, WorktreeInfo,
};

/// Applies per-session launch values over the config defaults for
/// `instance.tool`. Empty strings and `None` count as unset. Command priority:
/// per-session > `agent_command_override` > `custom_agents` > the value already
/// on `instance`.
pub(crate) fn apply_agent_launch_config(
    instance: &mut Instance,
    session: &super::config::SessionConfig,
    extra_args: &str,
    command_override: &str,
    yolo_mode: Option<bool>,
) {
    let extra = match extra_args {
        "" => session
            .agent_extra_args
            .get(&instance.tool)
            .map_or("", String::as_str),
        set => set,
    };
    if !extra.is_empty() {
        instance.extra_args = extra.to_string();
    }

    let command = match command_override {
        "" => session.resolve_tool_command(&instance.tool),
        set => set.to_string(),
    };
    if !command.is_empty() {
        instance.command = command;
    }

    instance.yolo_mode = yolo_mode.unwrap_or(session.yolo_mode_default);
}

/// Parameters for creating a new session instance.
#[derive(Debug, Clone)]
pub struct InstanceParams {
    pub title: String,
    /// `title` was typed by the user, so the agent may be given it as its own session name.
    pub title_typed: bool,
    pub path: String,
    pub group: String,
    pub tool: String,
    pub worktree_enabled: bool,
    pub worktree_branch: Option<String>,
    pub create_new_branch: bool,
    /// Branch to base a freshly-created worktree branch on.
    pub base_branch: Option<String>,
    pub sandbox: bool,
    /// The sandbox image to use. Required when sandbox is true.
    pub sandbox_image: String,
    pub yolo_mode: bool,
    /// Additional environment entries for the container.
    /// `KEY` = pass through from host, `KEY=VALUE` = set explicitly.
    pub extra_env: Vec<String>,
    /// Extra arguments to append after the agent binary
    pub extra_args: String,
    /// Command override for the agent binary (replaces the default binary)
    pub command_override: String,
    /// Additional repository paths for multi-repo workspace mode
    pub extra_repo_paths: Vec<String>,
    /// Per-repo base branches as `(selector, base)` pairs, from `aoe add --repo-base
    /// <selector>=<ref>` or the web wizard.
    pub repo_base_branches: Vec<(String, String)>,
    /// Scratch session: ignore `path`, provision a fresh directory under `<app_dir>/scratch/<id>/`,
    /// and persist `instance.scratch = true` so the deletion path removes the directory.
    pub scratch: bool,
    /// One-shot fork seed. When `Some`, the freshly-built instance is set up
    /// to fork its parent on first launch instead of starting fresh.
    pub fork_seed: Option<crate::session::ForkSeed>,
}

/// A prepared instance and its original publication custody.
pub struct BuildResult {
    pub instance: Instance,
    /// Non-fatal warnings from worktree/workspace creation. Callers should
    /// surface these to the user (post-checkout hook failures etc.).
    pub warnings: Vec<String>,
    /// Original filesystem custody for builds that reserve paths before effects.
    pub creation_intent: std::sync::Arc<CreationIntent>,
}

/// Result of creating a multi-repo workspace.
pub struct WorkspaceResult {
    pub workspace_info: WorkspaceInfo,
    pub workspace_path: PathBuf,
    /// Non-fatal warnings from worktree creation (e.g. post-checkout hook
    /// failures where the worktree itself was created successfully).
    pub warnings: Vec<String>,
    pub(crate) creation_intent: std::sync::Arc<CreationIntent>,
}

/// Live originals are retained independently of channels and UI lifetimes.
#[derive(Debug)]
pub struct CreationCustody {
    storage: std::sync::Arc<super::Storage>,
    admitted: Instance,
    state: std::sync::Mutex<CreationCustodyState>,
}

#[derive(Debug, Default)]
struct CreationCustodyState {
    intent: Option<std::sync::Arc<CreationIntent>>,
    ready: Option<CreationReady>,
    published: Option<Instance>,
    withdrawn: Option<CreationWithdrawalAck>,
}

#[derive(Debug, Clone)]
pub struct CreationReady {
    pub instance: Instance,
    pub warnings: Vec<String>,
    pub on_launch_hooks_ran: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreationUndoVerdict {
    NotReserved,
    RequiresOriginalProofCheck,
    Withdrawn,
    AlreadyPublished,
}

fn creation_originals() -> &'static std::sync::Mutex<Vec<std::sync::Arc<CreationCustody>>> {
    static ORIGINALS: std::sync::OnceLock<std::sync::Mutex<Vec<std::sync::Arc<CreationCustody>>>> =
        std::sync::OnceLock::new();
    ORIGINALS.get_or_init(Default::default)
}

impl CreationCustody {
    pub fn register(
        storage: std::sync::Arc<super::Storage>,
        admitted: &Instance,
    ) -> Result<std::sync::Arc<Self>> {
        if let Some(origin) = &admitted.storage_origin {
            anyhow::ensure!(
                storage.same_origin_as(origin),
                "creation admission changed its original profile"
            );
        }
        let mut originals = creation_originals()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(original) = originals.iter().find(|entry| {
            entry.admitted.id == admitted.id
                && entry.admitted.created_at == admitted.created_at
                && entry.storage.same_origin_as(&storage)
        }) {
            return Ok(original.clone());
        }
        storage.verify_profile_identity()?;
        let original = std::sync::Arc::new(Self {
            storage,
            admitted: admitted.clone(),
            state: Default::default(),
        });
        originals.push(original.clone());
        Ok(original)
    }

    /// Routing only: returned entries already own their original capabilities.
    pub fn retained() -> Vec<std::sync::Arc<Self>> {
        creation_originals()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn session_id(&self) -> &str {
        &self.admitted.id
    }
    pub fn storage(&self) -> &std::sync::Arc<super::Storage> {
        &self.storage
    }
    pub fn created_at(&self) -> chrono::DateTime<Utc> {
        self.admitted.created_at
    }

    pub fn generation(&self) -> Option<u64> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .intent
            .as_ref()
            .map(|intent| intent.acknowledged.lifecycle_generation)
    }

    pub fn ready(&self) -> Option<CreationReady> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .ready
            .clone()
    }

    pub fn retain_ready(&self, mut ready: CreationReady) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let intent = state
            .intent
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("creation has no original reserve acknowledgement"))?;
        anyhow::ensure!(
            ready.instance.id == self.admitted.id
                && ready.instance.created_at == self.admitted.created_at,
            "ready result changed its admitted identity"
        );
        intent.refresh_prepared(&ready.instance)?;
        ready.warnings.extend(intent.native_warnings()?);
        anyhow::ensure!(state.published.is_none(), "creation is already published");
        state.ready = Some(ready);
        Ok(())
    }

    /// Explicit publication only; never launches Git, hooks, containers or attach.
    pub fn retry_publication(&self) -> Result<Instance> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(published) = &state.published {
            return Ok(published.clone());
        }
        let ready = state.ready.as_ref().ok_or_else(|| {
            anyhow::anyhow!("creation has no complete prepared result; effects must not be rerun")
        })?;
        let intent = state
            .intent
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("original creation custody is unavailable"))?;
        let published = intent.publish(&ready.instance)?;
        state.published = Some(published.clone());
        Ok(published)
    }

    /// No deletion authority is inferred from absent runner journals or CLI exit.
    pub fn undo_verdict(&self) -> CreationUndoVerdict {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.withdrawn.is_some() {
            CreationUndoVerdict::Withdrawn
        } else if state.published.is_some() {
            CreationUndoVerdict::AlreadyPublished
        } else if state.intent.is_none() {
            CreationUndoVerdict::NotReserved
        } else {
            CreationUndoVerdict::RequiresOriginalProofCheck
        }
    }
}

/// Opaque producer acknowledgement, created only after physical Undo and same-Create withdrawal.
#[derive(Clone, Debug)]
pub struct CreationWithdrawalAck {
    storage: std::sync::Arc<super::Storage>,
    id: String,
    created_at: chrono::DateTime<Utc>,
    generation: u64,
}
impl CreationWithdrawalAck {
    pub fn session_id(&self) -> &str {
        &self.id
    }
    pub fn created_at(&self) -> chrono::DateTime<Utc> {
        self.created_at
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn matches_original(
        &self,
        storage: &super::Storage,
        id: &str,
        created_at: chrono::DateTime<Utc>,
        generation: u64,
    ) -> Result<bool> {
        self.storage.verify_profile_identity()?;
        storage.verify_profile_identity()?;
        Ok(self.storage.same_origin_as(storage)
            && self.id == id
            && self.created_at == created_at
            && self.generation == generation)
    }
}

impl CreationCustody {
    pub fn matches_original(
        &self,
        storage: &super::Storage,
        id: &str,
        created_at: chrono::DateTime<Utc>,
        generation: u64,
    ) -> Result<bool> {
        self.storage.verify_profile_identity()?;
        storage.verify_profile_identity()?;
        if !self.storage.same_origin_as(storage)
            || self.admitted.id != id
            || self.admitted.created_at != created_at
        {
            return Ok(false);
        }
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(ack) = &state.withdrawn {
            return ack.matches_original(storage, id, created_at, generation);
        }
        let Some(intent) = &state.intent else {
            return Ok(false);
        };
        if intent.acknowledged.lifecycle_generation != generation {
            return Ok(false);
        }
        let Some(row) = self.storage.load()?.into_iter().find(|row| row.id == id) else {
            return Ok(false);
        };
        if state.published.is_none() {
            intent.validate_row(&row)?;
        }
        Ok(row.created_at == created_at && row.lifecycle_generation == generation)
    }

    pub fn withdraw(&self) -> Result<CreationWithdrawalAck> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(ack) = &state.withdrawn {
            self.storage.verify_profile_identity()?;
            return Ok(ack.clone());
        }
        anyhow::ensure!(
            state.published.is_none(),
            "published creation cannot be withdrawn"
        );
        let intent = state
            .intent
            .as_ref()
            .context("original Creating reservation is unavailable")?;
        intent.undo_original()?;
        let ack = CreationWithdrawalAck {
            storage: self.storage.clone(),
            id: self.admitted.id.clone(),
            created_at: self.admitted.created_at,
            generation: intent.acknowledged.lifecycle_generation,
        };
        state.ready = None;
        state.intent = None;
        state.withdrawn = Some(ack.clone());
        Ok(ack)
    }
}

/// Borrowed physical namespace of this same original Create. The constructor
/// accepts only its producer-held PFD; no pathname can manufacture this token.
pub(crate) struct AnchoredDir {
    owner: std::sync::Arc<super::runner_journal::OwnedStop>,
    file: std::fs::File,
    path: PathBuf,
    identity: super::DirectoryIdentity,
}
impl AnchoredDir {
    pub(super) fn from_original(
        intent: &CreationIntent,
        directory: &super::AnchoredDir,
        identity: super::DirectoryIdentity,
    ) -> Result<Self> {
        let file = directory.duplicate_native_file()?;
        anyhow::ensure!(
            super::DirectoryIdentity::from_metadata(&file.metadata()?) == identity
                && identity.is_durable(),
            "original native filesystem PFD changed"
        );
        Ok(Self {
            owner: intent
                .owned_create
                .get()
                .context("original Create native custody is unavailable")?
                .clone(),
            file,
            path: directory.path().to_path_buf(),
            identity,
        })
    }
    pub(crate) fn native_owner(&self) -> &std::sync::Arc<super::runner_journal::OwnedStop> {
        &self.owner
    }
    pub(crate) fn native_file(&self) -> &std::fs::File {
        &self.file
    }
    pub(crate) fn native_path(&self) -> &std::path::Path {
        &self.path
    }
    pub(crate) fn native_identity(&self) -> super::DirectoryIdentity {
        self.identity
    }
}

/// The canonical original Create and its pre-effect resource custody.
pub struct CreationIntent {
    storage: std::sync::Arc<super::Storage>,
    acknowledged: Instance,
    ready_status: super::Status,
    paths: Vec<PathBuf>,
    owned_create: std::sync::OnceLock<std::sync::Arc<super::runner_journal::OwnedStop>>,
    undo: std::sync::Mutex<super::creation_undo::CreationUndo>,
}

impl std::fmt::Debug for CreationIntent {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CreationIntent")
            .field("session_id", &self.acknowledged.id)
            .finish_non_exhaustive()
    }
}

pub fn run_owned_create_bootstrap() -> Result<()> {
    super::runner_journal::native_create::bootstrap_child()
}

impl CreationIntent {
    pub(crate) fn borrow_owned_create(
        &self,
    ) -> Result<std::sync::Arc<super::runner_journal::OwnedStop>> {
        self.owned_create
            .get()
            .cloned()
            .context("original Create producer acknowledgement is unavailable")
    }

    /// Publish the prepared build through its original physical profile.
    pub fn publish(&self, prepared: &Instance) -> Result<Instance> {
        self.ensure_native_commands_observed()?;
        let _workspace = super::acquire_session_workspace_claim_lock()?;
        let _identity = super::acquire_session_identity_lock()?;
        self.storage.verify_profile_identity()?;
        super::validate_managed_workspace(prepared).map_err(anyhow::Error::msg)?;
        let manages_worktree = prepared
            .worktree_info
            .as_ref()
            .is_some_and(|info| info.managed_by_aoe)
            || prepared.workspace_info.is_some();
        if manages_worktree {
            let mut paths = vec![PathBuf::from(&prepared.project_path)];
            paths.extend(
                prepared
                    .all_repos()
                    .iter()
                    .map(|repo| PathBuf::from(&repo.worktree_path)),
            );
            super::deletion::ensure_unclaimed_paths(
                super::deletion::SessionPathOwner {
                    profile: self.storage.profile(),
                    session_id: &prepared.id,
                },
                &paths,
            )
            .map_err(anyhow::Error::msg)?;
        }
        publish_prepared_creation_under_workspace_claim_lock(
            self.storage(),
            prepared,
            self,
            |rows, groups| {
                if !prepared.group_path.is_empty() {
                    let mut tree = super::GroupTree::new_with_groups(rows, groups);
                    tree.create_group(&prepared.group_path);
                    *groups = tree.get_all_groups();
                }
                Ok(())
            },
        )
    }
    pub(crate) fn reserve(
        storage: &super::Storage,
        prepared: &mut Instance,
    ) -> Result<std::sync::Arc<Self>> {
        let paths = prepared
            .durable_worktree_paths()
            .map(PathBuf::from)
            .collect();
        Self::reserve_paths(storage, prepared, paths)
    }

    pub(crate) fn reserve_metadata(
        storage: &super::Storage,
        prepared: &mut Instance,
    ) -> Result<std::sync::Arc<Self>> {
        Self::reserve_paths(storage, prepared, Vec::new())
    }

    fn reserve_paths(
        storage: &super::Storage,
        prepared: &mut Instance,
        paths: Vec<PathBuf>,
    ) -> Result<std::sync::Arc<Self>> {
        let custody = CreationCustody::register(std::sync::Arc::new(storage.clone()), prepared)?;
        let mut custody_state = custody.state.lock().unwrap_or_else(|e| e.into_inner());
        anyhow::ensure!(
            custody_state.intent.is_none(),
            "original creation already reserved; retry publication instead"
        );
        if let Some(origin) = &prepared.storage_origin {
            anyhow::ensure!(
                storage.same_origin_as(origin),
                "creation changed its physical profile"
            );
        } else {
            prepared.storage_origin = Some(std::sync::Arc::new(storage.clone()));
        }
        prepared.source_profile = storage.profile().to_owned();
        let mut reserved = prepared.clone();
        let ready_status = prepared.status;
        reserved.status = super::Status::Creating;
        reserved.try_acquire_lifecycle_reservation(
            super::LifecycleOperation::Create,
            Instance::LIFECYCLE_RESERVATION_TTL,
            Utc::now(),
        )?;
        let custodian = crate::process::OriginalCustodianBirth::capture(storage, &reserved)?;
        let reservation = reserved.lifecycle_reservation.as_mut().unwrap();
        reservation.path_claims = super::WorktreePathClaims::Pending(paths.clone());
        reservation.custodian = Some(custodian);
        // Pure snapshot, before canonical reserve and before any native or FS effect.
        let undo = super::creation_undo::CreationUndo::freeze(&reserved, &paths)?;
        let acknowledged = storage.update(|rows, _groups| {
            if super::is_duplicate_session(
                rows.iter(),
                &reserved.title,
                &reserved.project_path,
                None,
            ) {
                return Err(super::duplicate_session_error(&reserved.title));
            }
            anyhow::ensure!(
                !rows.iter().any(|row| row.id == reserved.id),
                "creation identity is already owned"
            );
            if !paths.is_empty() {
                let claims = super::deletion::PathClaimIndex::load_for_writer(
                    std::slice::from_ref(storage),
                )?;
                let profile = claims.writer_profile(storage)?;
                claims.ensure_unclaimed(profile, &reserved.id, &paths)?;
            }
            rows.push(reserved.clone());
            Ok(reserved)
        })?;
        prepared.lifecycle_generation = acknowledged.lifecycle_generation;
        prepared.lifecycle_reservation = acknowledged.lifecycle_reservation.clone();
        let intent = std::sync::Arc::new(Self {
            storage: std::sync::Arc::clone(&custody.storage),
            acknowledged,
            ready_status,
            paths,
            undo: std::sync::Mutex::new(undo),
            owned_create: Default::default(),
        });
        custody_state.intent = Some(intent.clone());
        let owned_create = super::runner_journal::OwnedStop::from_claim(
            &custody.storage,
            &intent.acknowledged,
            super::LifecycleOperation::Create,
            intent.acknowledged.lifecycle_generation,
        )?;
        intent
            .owned_create
            .set(owned_create)
            .map_err(|_| anyhow::anyhow!("creation native original was already installed"))?;
        Ok(intent)
    }

    /// Routing only; a DTO cannot reconstruct missing historical originals.
    pub(crate) fn original_for(instance: &Instance) -> Result<Option<std::sync::Arc<Self>>> {
        if !instance
            .lifecycle_reservation
            .as_ref()
            .is_some_and(|lease| lease.op == super::LifecycleOperation::Create)
        {
            return Ok(None);
        }
        let storage = instance.original_storage()?;
        let original = CreationCustody::retained()
            .into_iter()
            .find(|entry| {
                entry.admitted.id == instance.id
                    && entry.admitted.created_at == instance.created_at
                    && entry.storage.same_origin_as(&storage)
            })
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "original Creating custodian is unavailable; native commands remain protected"
                )
            })?;
        let intent = original
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .intent
            .clone()
            .ok_or_else(|| anyhow::anyhow!("original Creating reservation is unavailable"))?;
        anyhow::ensure!(
            intent.acknowledged.lifecycle_generation == instance.lifecycle_generation,
            "original Creating counter changed"
        );
        intent.storage.verify_profile_identity()?;
        Ok(Some(intent))
    }

    pub(crate) fn require_worktree_plan(
        &self,
        repo: &std::path::Path,
        branch: &str,
        path: &std::path::Path,
    ) -> Result<()> {
        self.undo
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .require_worktree(repo, branch, path)
    }
    pub(crate) fn acknowledge_created_branch(
        &self,
        repo: &std::path::Path,
        branch: &str,
        produced: git2::Oid,
    ) -> Result<()> {
        self.undo
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .acknowledge_created_branch(repo, branch, produced)
    }
    pub(crate) fn owned_git_command(
        &self,
        cwd: &std::path::Path,
    ) -> Result<super::runner_journal::OwnedCreateCommand> {
        let mut command = self.owned_command("git")?;
        self.undo
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .bind_command_directory(self, &mut command, cwd, true)?;
        Ok(command)
    }
    pub(crate) fn owned_hook_command(
        &self,
        program: impl AsRef<std::ffi::OsStr>,
        cwd: &std::path::Path,
    ) -> Result<super::runner_journal::OwnedCreateCommand> {
        let mut command = self.owned_command(program)?;
        self.undo
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .bind_command_directory(self, &mut command, cwd, false)?;
        Ok(command)
    }
    pub(crate) fn allocate_worktree_bootstrap(
        &self,
        repo: &std::path::Path,
        branch: &str,
        path: &std::path::Path,
        reason: &str,
    ) -> Result<()> {
        let _fences = self.original_undo_fences()?;
        self.undo
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .allocate_worktree_bootstrap(repo, branch, path, reason)
    }
    pub(crate) fn begin_worktree_effect(
        &self,
        repo: &std::path::Path,
        branch: &str,
        path: &std::path::Path,
    ) -> Result<super::creation_undo::OwnedWorktreeLayout> {
        let _fences = self.original_undo_fences()?;
        self.undo
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .begin_worktree_effect(self, repo, branch, path)
    }
    pub(crate) fn begin_checkout_command(
        &self,
        repo: &std::path::Path,
        branch: &str,
        path: &std::path::Path,
    ) -> Result<()> {
        self.undo
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .begin_checkout_command(repo, branch, path)
    }
    pub(crate) fn original_worktree_lock(&self, path: &std::path::Path) -> Result<()> {
        self.undo
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .original_worktree_lock(path)
    }
    pub(crate) fn retain_submodule_domain(&self, path: &std::path::Path) -> Result<()> {
        self.undo
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .retain_submodule_domain(path)
    }
    pub(crate) fn prepare_tracking(&self, repo: &std::path::Path, branch: &str) -> Result<()> {
        self.undo
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .prepare_tracking(repo, branch)
    }
    pub(crate) fn acknowledge_tracking(
        &self,
        repo: &std::path::Path,
        branch: &str,
        remote: &str,
        merge: &str,
    ) -> Result<()> {
        self.undo
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .acknowledge_tracking(repo, branch, remote, merge)
    }
    pub(crate) fn acknowledge_worktree(
        &self,
        repo: &std::path::Path,
        branch: &str,
        path: &std::path::Path,
        complete: bool,
    ) -> Result<()> {
        self.undo
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .acknowledge_worktree(repo, branch, path, complete)
    }
    pub(crate) fn provision_directory(&self, path: &std::path::Path) -> Result<()> {
        self.undo
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .provision(path)
    }
    pub(crate) fn retain_container_domain(&self, instance: &Instance) -> Result<()> {
        self.undo
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain_container(instance)
    }
    pub(crate) fn retain_container_goal(&self, argv: &[String]) {
        self.undo
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain_container_goal(argv);
    }
    pub(crate) fn acknowledge_container_result(&self, id: &str) {
        self.undo
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .acknowledge_container_result(id);
    }
    pub(crate) fn original_undo_fences(&self) -> Result<CleanupOwnershipLocks> {
        let fences = CleanupOwnershipLocks::acquire()?;
        self.storage.verify_profile_identity()?;
        let canonical = self
            .storage
            .load()?
            .into_iter()
            .find(|row| row.id == self.session_id())
            .ok_or_else(|| anyhow::anyhow!("original Creating row is unavailable"))?;
        self.validate_row(&canonical)?;
        if !self.paths.is_empty() {
            super::deletion::ensure_unclaimed_paths(
                super::deletion::SessionPathOwner {
                    profile: self.storage.profile(),
                    session_id: self.session_id(),
                },
                &self.paths,
            )
            .map_err(anyhow::Error::msg)?;
        }
        Ok(fences)
    }

    pub(crate) fn undo_original(&self) -> Result<()> {
        // Native settlement must precede acquiring the filesystem Undo fences.
        self.begin_owned_withdrawal()?;
        self.retire_owned_commands()?;
        self.undo
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .undo(self)?;
        self.retire_owned_commands()?;
        let _fences = self.original_undo_fences()?;
        self.storage
            .complete_creation_under_workspace_claim_lock(self, |rows, _| {
                let row = rows
                    .iter()
                    .find(|row| row.id == self.session_id())
                    .ok_or_else(|| anyhow::anyhow!("original Creating row disappeared"))?;
                self.validate_row(row)?;
                rows.retain(|row| row.id != self.session_id());
                Ok(())
            })
    }

    pub(crate) fn storage(&self) -> &super::Storage {
        &self.storage
    }
    pub(crate) fn session_id(&self) -> &str {
        &self.acknowledged.id
    }

    pub(crate) fn validate_row(&self, row: &Instance) -> Result<()> {
        self.storage.verify_profile_identity()?;
        anyhow::ensure!(
            row.id == self.acknowledged.id
                && row.created_at == self.acknowledged.created_at
                && row.lifecycle_reservation == self.acknowledged.lifecycle_reservation
                && self.same_filesystem_plan(row),
            "creation filesystem custody was superseded"
        );
        anyhow::ensure!(row.lifecycle_reservation.as_ref().is_some_and(|lease| {
            lease.op == super::LifecycleOperation::Create
                && matches!(&lease.path_claims, super::WorktreePathClaims::Pending(paths) if paths == &self.paths)
        }), "creation has no acknowledged complete filesystem plan");
        Ok(())
    }

    fn same_filesystem_plan(&self, row: &Instance) -> bool {
        let original = &self.acknowledged;
        fn worktree(row: &Instance) -> Option<(&str, &str, bool, Option<&str>)> {
            row.worktree_info.as_ref().map(|info| {
                (
                    info.branch.as_str(),
                    info.main_repo_path.as_str(),
                    info.managed_by_aoe,
                    info.base_branch.as_deref(),
                )
            })
        }
        if row.project_path != original.project_path
            || row.scratch != original.scratch
            || worktree(row) != worktree(original)
        {
            return false;
        }
        match (&row.workspace_info, &original.workspace_info) {
            (None, None) => true,
            (Some(current), Some(original)) => {
                current.workspace_dir == original.workspace_dir
                    && current.branch == original.branch
                    && current.cleanup_on_delete == original.cleanup_on_delete
                    && current
                        .repos
                        .iter()
                        .map(|repo| {
                            (
                                &repo.name,
                                &repo.source_path,
                                &repo.branch,
                                &repo.worktree_path,
                                &repo.main_repo_path,
                                repo.managed_by_aoe,
                                repo.branch_preexisting,
                                &repo.base_branch,
                            )
                        })
                        .eq(original.repos.iter().map(|repo| {
                            (
                                &repo.name,
                                &repo.source_path,
                                &repo.branch,
                                &repo.worktree_path,
                                &repo.main_repo_path,
                                repo.managed_by_aoe,
                                repo.branch_preexisting,
                                &repo.base_branch,
                            )
                        }))
            }
            _ => false,
        }
    }

    fn prepared_row(&self, canonical: &Instance, prepared: &Instance) -> Result<Instance> {
        self.validate_row(canonical)?;
        anyhow::ensure!(
            prepared.id == canonical.id
                && prepared.created_at == canonical.created_at
                && self.same_filesystem_plan(prepared),
            "prepared creation changed its immutable filesystem plan"
        );
        anyhow::ensure!(
            prepared
                .storage_origin
                .as_ref()
                .is_some_and(|origin| self.storage.same_origin_as(origin))
                && prepared.source_profile == self.storage.profile(),
            "prepared creation changed its original physical profile"
        );
        let mut row = prepared.clone();
        row.runner_journal = canonical.runner_journal.clone();
        row.active_execution = canonical.active_execution.clone();
        row.lifecycle_generation = canonical.lifecycle_generation;
        row.lifecycle_reservation = canonical.lifecycle_reservation.clone();
        Ok(row)
    }

    pub(crate) fn refresh_prepared(&self, prepared: &Instance) -> Result<()> {
        // The true producer retains the actual metadata CAS ACK as its current
        // projection. Later Git/hooks freeze this effective goal without
        // recapturing a DTO, renewing Create, or discarding prior native births.
        self.borrow_owned_create()?.update_projection(
            |row| {
                let mut refreshed = self.prepared_row(row, prepared)?;
                refreshed.status = super::Status::Creating;
                *row = refreshed;
                Ok(())
            },
            |_| Ok(()),
        )
    }

    pub(crate) fn publish_under_workspace_claim_lock<F, R>(
        &self,
        prepared: &Instance,
        publish: F,
    ) -> Result<R>
    where
        F: FnOnce(&mut Vec<Instance>, &mut Vec<super::Group>, Instance) -> Result<R>,
    {
        self.storage
            .complete_creation_under_workspace_claim_lock(self, |rows, groups| {
                let canonical = rows
                    .iter()
                    .find(|row| row.id == self.session_id())
                    .ok_or_else(|| anyhow::anyhow!("creation filesystem owner disappeared"))?;
                let mut committed = self.prepared_row(canonical, prepared)?;
                committed.lifecycle_reservation = None;
                if committed.status == super::Status::Creating {
                    committed.status = self.ready_status;
                }
                publish(rows, groups, committed)
            })
    }
}

/// Normalize a base-branch string, treating empty/whitespace as unset.
fn normalize_base(s: Option<&str>) -> Option<String> {
    s.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Resolve a repo's effective base branch with precedence: explicit session base > per-project
/// default > global/profile default.
pub(crate) fn resolve_base_branch(
    session: Option<&str>,
    project: Option<&str>,
    global: Option<&str>,
) -> Option<String> {
    normalize_base(session)
        .or_else(|| normalize_base(project))
        .or_else(|| normalize_base(global))
}

/// Resolve one repo's effective base branch, consulting its registered per-project default.
fn resolve_repo_base_branch(
    repo_path: &std::path::Path,
    session: Option<&str>,
    project_bases: &std::collections::HashMap<String, String>,
    global: Option<&str>,
) -> Option<String> {
    let main_repo =
        GitWorktree::find_main_repo(repo_path).unwrap_or_else(|_| repo_path.to_path_buf());
    let key = crate::session::projects::canonical_key(&main_repo.to_string_lossy());
    let project = project_bases.get(&key).map(String::as_str);
    resolve_base_branch(session, project, global)
}

/// Match `(selector, base)` pairs to the repos a session is being built from.
pub(crate) fn resolve_repo_base_selectors(
    repos: &[PathBuf],
    pairs: &[(String, String)],
) -> Result<std::collections::HashMap<PathBuf, String>> {
    let mut out = std::collections::HashMap::new();
    for (selector, base) in pairs {
        let sel = selector.trim();
        let Some(base) = normalize_base(Some(base)) else {
            bail!("No base branch given for repo '{}'", sel);
        };
        let matches: Vec<&PathBuf> = repos
            .iter()
            .filter(|p| {
                p.as_os_str() == sel
                    || p.file_name()
                        .is_some_and(|n| n == std::ffi::OsStr::new(sel))
            })
            .collect();
        match matches.as_slice() {
            [one] => {
                if out.insert((*one).clone(), base).is_some() {
                    bail!("Repo '{}' was given a base branch twice", sel);
                }
            }
            [] => {
                let known: Vec<String> = repos
                    .iter()
                    .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
                    .collect();
                bail!(
                    "No repo named '{}' in this session. Available: {}",
                    sel,
                    known.join(", ")
                );
            }
            _ => bail!(
                "Repo name '{}' is ambiguous; pass the full path instead",
                sel
            ),
        }
    }
    Ok(out)
}

/// Map of canonical repo path to configured default base branch for every registered project
/// (global + profile) that sets one.
pub(crate) fn project_base_branches(profile: &str) -> std::collections::HashMap<String, String> {
    crate::session::projects::load_merged(profile)
        .unwrap_or_else(|e| {
            // Don't fork worktrees from the wrong base in silence: if the registry can't be read,
            // log it so the missing per-project defaults are explainable instead of mysterious.
            tracing::warn!(
                target: "session.create",
                "Failed to load project registry for base-branch defaults; \
                 repos fall back to the global default: {e}"
            );
            Vec::new()
        })
        .into_iter()
        .filter_map(|p| {
            let base = p.default_base_branch?;
            let base = base.trim().to_string();
            if base.is_empty() {
                None
            } else {
                Some((crate::session::projects::canonical_key(&p.path), base))
            }
        })
        .collect()
}

/// One repository in a multi-repo workspace, paired with the base branch its freshly-created
/// worktree branch should fork from.
pub struct WorkspaceRepoSpec {
    pub path: PathBuf,
    pub base_branch: Option<String>,
}

/// Create a multi-repo workspace with worktrees for each repository.
pub(crate) fn create_workspace(
    primary: &WorkspaceRepoSpec,
    extra_repos: &[WorkspaceRepoSpec],
    branch: &str,
    create_new_branch: bool,
    workspace_template: &str,
    init_submodules: bool,
    prepared: &mut Instance,
) -> Result<WorkspaceResult> {
    let original_storage = prepared.original_storage()?;
    let result = (|| -> Result<WorkspaceResult> {
        let storage = prepared.original_storage()?;
        let primary_main_repo = GitWorktree::find_main_repo(&primary.path)?;
        let primary_git_wt = GitWorktree::new(primary_main_repo)?;

        let session_id_short = &prepared.id[..8];

        let workspace_path =
            primary_git_wt.compute_path(branch, workspace_template, session_id_short)?;
        anyhow::ensure!(
            !workspace_path.try_exists()?,
            "workspace destination already exists"
        );
        let workspace_dir = workspace_path.to_string_lossy().to_string();

        // (canonicalized path, resolved base branch) for the primary repo followed by every extra repo.
        let all_repos: Vec<(PathBuf, Option<String>)> =
            std::iter::once((primary.path.clone(), primary.base_branch.clone()))
                .chain(extra_repos.iter().map(|r| {
                    (
                        r.path.canonicalize().unwrap_or_else(|_| r.path.clone()),
                        r.base_branch.clone(),
                    )
                }))
                .collect();

        // Check for duplicate repo directory names
        let mut seen_names = std::collections::HashSet::new();
        for (repo_path, _) in &all_repos {
            let name = repo_path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "repo".to_string());
            if !seen_names.insert(name.clone()) {
                bail!(
                    "Duplicate repository name '{}' in workspace\n\
                 Tip: Rename one of the directories to avoid the collision",
                    name
                );
            }
        }

        // Resolve every repository before reserving or mutating a path.
        struct RepoPlan {
            repo_path: PathBuf,
            repo_name: String,
            main_repo_path: PathBuf,
            worktree_subdir: PathBuf,
            base_branch: Option<String>,
        }
        let mut plans: Vec<RepoPlan> = Vec::with_capacity(all_repos.len());
        for (repo_path, base_branch) in &all_repos {
            if !GitWorktree::is_git_repo(repo_path) {
                bail!(
                    "Path is not in a git repository: {}\n\
                 Tip: All --repo paths must be git repositories",
                    repo_path.display()
                );
            }

            let main_repo_path_raw = GitWorktree::find_main_repo(repo_path)?;
            let main_repo_path = main_repo_path_raw
                .canonicalize()
                .unwrap_or(main_repo_path_raw);

            let repo_name = repo_path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "repo".to_string());

            let worktree_subdir = workspace_path.join(&repo_name);

            plans.push(RepoPlan {
                repo_path: repo_path.clone(),
                repo_name,
                main_repo_path,
                worktree_subdir,
                base_branch: base_branch.clone(),
            });
        }
        let workspace_info = WorkspaceInfo {
            branch: branch.to_string(),
            workspace_dir,
            created_at: Utc::now(),
            cleanup_on_delete: true,
            repos: plans
                .iter()
                .map(|plan| WorkspaceRepo {
                    name: plan.repo_name.clone(),
                    source_path: plan.repo_path.to_string_lossy().into_owned(),
                    branch: branch.to_string(),
                    worktree_path: plan.worktree_subdir.to_string_lossy().into_owned(),
                    main_repo_path: plan.main_repo_path.to_string_lossy().into_owned(),
                    managed_by_aoe: true,
                    branch_preexisting: false,
                    base_branch: create_new_branch
                        .then(|| plan.base_branch.clone())
                        .flatten(),
                    base_branch_override: None,
                })
                .collect(),
        };
        prepared.project_path = workspace_path.to_string_lossy().into_owned();
        prepared.workspace_info = Some(workspace_info.clone());
        let creation_intent = CreationIntent::reserve(&storage, prepared)?;
        creation_intent.provision_directory(&workspace_path)?;

        // Run create_worktree for every repo concurrently.
        let create_start = std::time::Instant::now();
        let parallel_results: Vec<std::result::Result<Vec<String>, String>> =
            std::thread::scope(|scope| {
                let handles: Vec<_> = plans
                    .iter()
                    .map(|plan| {
                        let branch = branch.to_string();
                        let base = plan.base_branch.clone();
                        let main_repo_path = plan.main_repo_path.clone();
                        let worktree_subdir = plan.worktree_subdir.clone();
                        let repo_name = plan.repo_name.clone();
                        let creation_intent = creation_intent.clone();
                        scope.spawn(move || -> std::result::Result<Vec<String>, String> {
                            let repo_start = std::time::Instant::now();
                            let result = (|| -> std::result::Result<Vec<String>, String> {
                                let git_wt = GitWorktree::new(main_repo_path)
                                    .map_err(|e| format!("{}: {}", repo_name, e))?
                                    .with_init_submodules(init_submodules);
                                git_wt
                                    .create_worktree_owned(
                                        &branch,
                                        &worktree_subdir,
                                        create_new_branch,
                                        base.as_deref(),
                                        &creation_intent,
                                    )
                                    .map_err(|e| format!("{}: {}", repo_name, e))
                            })();
                            tracing::info!(target: "session.create",
                                "workspace create: repo={} elapsed={:?} ok={}",
                                repo_name,
                                repo_start.elapsed(),
                                result.is_ok()
                            );
                            result
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|h| match h.join() {
                        Ok(r) => r,
                        Err(_) => Err("worktree thread panicked".to_string()),
                    })
                    .collect()
            });
        tracing::info!(target: "session.create",
            "workspace create: {} repos completed in {:?}",
            plans.len(),
            create_start.elapsed()
        );

        let mut warnings: Vec<String> = Vec::new();
        let mut errors: Vec<String> = Vec::new();

        for result in parallel_results {
            match result {
                Ok(w) => {
                    warnings.extend(w);
                }
                Err(msg) => errors.push(msg),
            }
        }

        if !errors.is_empty() {
            tracing::warn!(target: "session.create", session = %prepared.id, "Retaining durable creation intent after an uncertain Git outcome");
            if errors.len() == 1 {
                bail!("Failed to create worktree for {}", errors.remove(0));
            } else {
                bail!(
                    "Failed to create worktrees ({} repos):\n  - {}",
                    errors.len(),
                    errors.join("\n  - ")
                );
            }
        }

        Ok(WorkspaceResult {
            workspace_info,
            workspace_path,
            warnings,
            creation_intent,
        })
    })();
    result.map_err(|error| finish_failed_creation(&original_storage, prepared, error))
}

/// Build an instance with all setup (worktree resolution, sandbox config).
pub fn build_instance(
    params: InstanceParams,
    existing_titles: &[&str],
    existing_branches: &[&str],
    storage: &super::Storage,
) -> Result<BuildResult> {
    build_instance_from_admitted(
        params,
        existing_titles,
        existing_branches,
        Instance::new("", ""),
        storage,
    )
}

pub(crate) fn build_instance_from_admitted(
    params: InstanceParams,
    existing_titles: &[&str],
    existing_branches: &[&str],
    instance: Instance,
    storage: &super::Storage,
) -> Result<BuildResult> {
    let admitted = instance.clone();
    prepare_admitted_instance(
        params,
        existing_titles,
        existing_branches,
        instance,
        storage,
    )
    .map_err(|error| finish_failed_creation(storage, &admitted, error))
}

/// Error cleanup only routes to a surviving original; it cannot mint one from a row.
pub(crate) fn finish_failed_creation(
    storage: &super::Storage,
    admitted: &Instance,
    error: anyhow::Error,
) -> anyhow::Error {
    let Some(original) = CreationCustody::retained().into_iter().find(|entry| {
        entry.admitted.id == admitted.id
            && entry.admitted.created_at == admitted.created_at
            && entry.storage.same_origin_as(storage)
    }) else {
        return error;
    };
    if original
        .state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .intent
        .is_none()
    {
        return error;
    }
    match original.withdraw() {
        Ok(ack) => error.context(format!(
            "Original Create {} at counter {} was retired and withdrawn",
            ack.session_id(),
            ack.generation()
        )),
        Err(proof) => error.context(format!(
            "Original Create {} and its resources remain retained: {proof:#}",
            admitted.id
        )),
    }
}

fn prepare_admitted_instance(
    params: InstanceParams,
    existing_titles: &[&str],
    existing_branches: &[&str],
    mut instance: Instance,
    storage: &super::Storage,
) -> Result<BuildResult> {
    let _custody = CreationCustody::register(std::sync::Arc::new(storage.clone()), &instance)?;
    storage.verify_profile_identity()?;
    let profile = storage.profile();
    instance.storage_origin = Some(std::sync::Arc::new(storage.clone()));
    instance.source_profile = profile.to_owned();

    // Host-only agents (e.g. settl) cannot run in a sandbox or use worktrees.
    let is_host_only = crate::agents::get_agent(&params.tool).is_some_and(|a| a.host_only);
    if is_host_only && params.sandbox {
        bail!(
            "{} can only run on the host, not in a sandbox.",
            params.tool
        );
    }
    if is_host_only && params.worktree_enabled {
        bail!("{} does not support worktree mode.", params.tool);
    }

    if params.scratch {
        if params.worktree_enabled {
            bail!("Cannot combine --scratch with worktree mode");
        }
        if !params.extra_repo_paths.is_empty() {
            bail!("Cannot combine --scratch with extra repository paths");
        }
    }

    if params.sandbox {
        let runtime = containers::get_container_runtime();
        if !runtime.is_available() {
            bail!("Container runtime is not installed. Please install a supported runtime to use sandbox mode.");
        }
        if !runtime.is_daemon_running() {
            bail!("Container runtime daemon is not running. Please start a supported runtime to use sandbox mode.");
        }
    }

    // Scratch sessions have no project repo, so config resolution falls back to global+profile
    // defaults (`Path::new("")` makes `resolve_config_with_repo` skip the repo-config layer
    // cleanly).
    let config_path = if params.scratch {
        std::path::PathBuf::new()
    } else {
        std::path::PathBuf::from(&params.path)
    };
    let config =
        super::config::repo_config::resolve_config_with_repo(profile, &config_path).unwrap_or_else(|e| {
            tracing::warn!(target: "session.create", "Failed to load config, using defaults: {}", e);
            Config::default()
        });

    let mut final_path = if params.scratch {
        // Provisioning happens after `Instance::new` so we can key the directory on the generated
        // instance id.
        String::new()
    } else {
        PathBuf::from(&params.path)
            .canonicalize()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| params.path.clone())
    };

    let mut worktree_info = None;
    let mut workspace_info = None;
    let mut warnings: Vec<String> = Vec::new();
    let mut creation_intent = None;
    let taken_branches = collect_taken_branches_for_derived_dedupe(
        existing_branches,
        &params.path,
        &params.extra_repo_paths,
        params.worktree_enabled,
        params.create_new_branch,
        params.scratch,
    );
    let final_title = resolve_title(
        &params.title,
        params.worktree_branch.as_deref(),
        params.worktree_enabled,
        existing_titles,
        &taken_branches,
    )?;
    let branch_source = resolve_worktree_branch(
        params.worktree_enabled,
        params.worktree_branch.as_deref(),
        &final_title,
    );

    let effective_worktree_branch: Option<String> = match branch_source {
        None => None,
        Some(BranchSource::Explicit(name)) => Some(name),
        Some(BranchSource::Derived(name)) => {
            if params.create_new_branch {
                Some(dedupe_branch_name(&name, &taken_branches))
            } else {
                Some(name)
            }
        }
    };

    instance.title = final_title;
    instance.first_launch_names_agent = params.title_typed;
    instance.group_path = params.group;
    instance.tool = params.tool.clone();
    instance.detect_as = config
        .session
        .agent_detect_as
        .get(&params.tool)
        .cloned()
        .unwrap_or_default();
    instance.command = crate::agents::get_agent(&params.tool)
        .filter(|a| a.set_default_command)
        .map(|a| a.binary.to_string())
        .unwrap_or_default();
    if let Some(notice) =
        crate::agents::get_agent(&params.tool).and_then(crate::agents::AgentDef::lifecycle_notice)
    {
        tracing::warn!(target: "session.builder", "agent '{}' is {notice}", params.tool);
    }
    apply_agent_launch_config(
        &mut instance,
        &config.session,
        &params.extra_args,
        &params.command_override,
        Some(params.yolo_mode),
    );
    if instance.command.trim().is_empty() && crate::agents::get_agent(&params.tool).is_none() {
        bail!(
            "No launch command resolved for custom agent '{}'. Config may have changed since validation.",
            params.tool
        );
    }

    if params.sandbox {
        // Surface env-resolution warnings up-front.
        let effective_env: &[String] = if params.extra_env.is_empty() {
            &config.sandbox.environment
        } else {
            &params.extra_env
        };
        warnings.extend(crate::session::validate_env_entries(effective_env));

        instance.sandbox_info = Some(SandboxInfo {
            enabled: true,
            container_id: None,
            image: params.sandbox_image.clone(),
            container_name: containers::DockerContainer::generate_name(&instance.id),
            extra_env: if params.extra_env.is_empty() {
                None
            } else {
                Some(params.extra_env.clone())
            },
            custom_instruction: config.sandbox.custom_instruction.clone(),
            before_start_env: Vec::new(),
            provider: None,
            container_workdir: None,
        });
    }

    if let Some(seed) = params.fork_seed {
        match seed {
            crate::session::ForkSeed::Terminal {
                parent,
                child_session_id,
                unattributed_parent_agent,
            } => {
                // Unattributed forks must still launch the parent's agent.
                if let Some(parent_agent) = unattributed_parent_agent.as_deref() {
                    let launched = Instance::execution_agent_for(
                        &instance.tool,
                        instance.get_tool_command(),
                        &config.session,
                    )
                    .map_err(anyhow::Error::msg)?;
                    crate::session::fork::ensure_child_matches_parent_agent(
                        Some(parent_agent),
                        launched.name,
                    )
                    .map_err(anyhow::Error::msg)?;
                }
                instance.agent_session_id = Some(child_session_id);
                instance.resume_intent = crate::session::ResumeIntent::Fork {
                    from: parent.session_id.clone(),
                };
                instance.resume_binding = Some(*parent);
            }
            crate::session::ForkSeed::Structured {
                parent_acp_session_id,
            } => {
                // Seed the structured fork handshake before first connect.
                instance.view = crate::session::View::Structured;
                instance.fork_pending = Some(parent_acp_session_id);
                instance.import_pending = Some(true);
            }
        }
    }
    if let Some(branch) = &effective_worktree_branch {
        if !params.extra_repo_paths.is_empty() {
            let primary_path = PathBuf::from(&params.path)
                .canonicalize()
                .unwrap_or_else(|_| PathBuf::from(&params.path));

            let session_base = params.base_branch.as_deref();
            let global_default = config.worktree.default_base_branch.as_deref();
            let project_bases = project_base_branches(profile);

            // An explicit per-repo base outranks every shared layer, which is the point: one repo
            // forks from develop while the others fork from their own epic branches.
            let mut all_paths = vec![primary_path.clone()];
            all_paths.extend(params.extra_repo_paths.iter().map(PathBuf::from));
            let per_repo = resolve_repo_base_selectors(&all_paths, &params.repo_base_branches)?;
            let base_for = |path: &PathBuf| {
                per_repo.get(path).cloned().or_else(|| {
                    // Every repo, including the launch repo, otherwise forks from its own
                    // registered per-project default when no explicit session base is given.
                    resolve_repo_base_branch(path, session_base, &project_bases, global_default)
                })
            };

            let primary = WorkspaceRepoSpec {
                base_branch: base_for(&primary_path),
                path: primary_path,
            };
            let extra_repos: Vec<WorkspaceRepoSpec> = params
                .extra_repo_paths
                .iter()
                .map(|p| {
                    let path = PathBuf::from(p);
                    WorkspaceRepoSpec {
                        base_branch: base_for(&path),
                        path,
                    }
                })
                .collect();

            let ws_result = create_workspace(
                &primary,
                &extra_repos,
                branch,
                params.create_new_branch,
                &config.worktree.workspace_path_template,
                config.worktree.init_submodules,
                &mut instance,
            )?;

            final_path = ws_result.workspace_path.to_string_lossy().to_string();
            workspace_info = Some(ws_result.workspace_info);
            creation_intent = Some(ws_result.creation_intent);
            warnings.extend(ws_result.warnings);
        } else {
            // Single worktree mode (existing logic)
            let path = PathBuf::from(&params.path);
            if !GitWorktree::is_git_repo(&path) {
                // Typed error (not a bare `bail!` string) so the web handler's whitelist forwards
                // an actionable message instead of the opaque "Failed to create session".
                return Err(anyhow::Error::new(GitError::NotAGitRepo).context(format!(
                    "Worktree mode requires a git repository, but this path is not one: {}\n\
                     Tip: start an in-place session (no worktree) here, or point at a git repository.",
                    path.display()
                )));
            }
            let main_repo_path_raw = GitWorktree::find_main_repo(&path)?;
            let main_repo_path = main_repo_path_raw
                .canonicalize()
                .unwrap_or(main_repo_path_raw);
            let git_wt = GitWorktree::new(main_repo_path.clone())?
                .with_init_submodules(config.worktree.init_submodules);

            // Choose appropriate template based on repo type (bare vs regular)
            // Use main_repo_path (not path) to correctly detect bare repos when running from a worktree
            let is_bare = GitWorktree::is_bare_repo(&main_repo_path);
            let template = if is_bare {
                &config.worktree.bare_repo_path_template
            } else {
                &config.worktree.path_template
            };

            if !params.create_new_branch {
                let existing_worktrees = git_wt.list_worktrees()?;
                if let Some(existing) = existing_worktrees
                    .iter()
                    .find(|wt| wt.branch.as_deref() == Some(branch))
                {
                    final_path = existing.path.to_string_lossy().to_string();
                    worktree_info = Some(WorktreeInfo {
                        branch: branch.clone(),
                        main_repo_path: main_repo_path.to_string_lossy().to_string(),
                        managed_by_aoe: false,
                        created_at: Utc::now(),
                        base_branch: None,
                    });
                } else {
                    let worktree_path = git_wt.compute_path(branch, template, &instance.id[..8])?;
                    anyhow::ensure!(
                        !worktree_path.try_exists()?,
                        "worktree destination already exists"
                    );
                    final_path = worktree_path.to_string_lossy().into_owned();
                    worktree_info = Some(WorktreeInfo {
                        branch: branch.clone(),
                        main_repo_path: main_repo_path.to_string_lossy().into_owned(),
                        managed_by_aoe: true,
                        created_at: Utc::now(),
                        base_branch: None,
                    });
                    instance.project_path.clone_from(&final_path);
                    instance.worktree_info.clone_from(&worktree_info);
                    creation_intent = Some(CreationIntent::reserve(storage, &mut instance)?);
                    let w = git_wt.create_worktree_owned(
                        branch,
                        &worktree_path,
                        false,
                        None,
                        creation_intent.as_ref().unwrap(),
                    )?;
                    warnings.extend(w);
                }
            } else {
                let worktree_path = git_wt.compute_path(branch, template, &instance.id[..8])?;

                if worktree_path.exists() {
                    return Err(GitError::WorktreeAlreadyExists(worktree_path.clone()).into());
                }

                // One repo, so a per-repo base can only name this one.
                let per_repo = resolve_repo_base_selectors(
                    std::slice::from_ref(&main_repo_path),
                    &params.repo_base_branches,
                )?;
                // The launch repo otherwise forks from its registered per-project default when no
                // explicit session base is given (then global/profile, then auto-detect).
                let project_bases = project_base_branches(profile);
                let base = per_repo.get(&main_repo_path).cloned().or_else(|| {
                    resolve_repo_base_branch(
                        &main_repo_path,
                        params.base_branch.as_deref(),
                        &project_bases,
                        config.worktree.default_base_branch.as_deref(),
                    )
                });

                final_path = worktree_path.to_string_lossy().into_owned();
                worktree_info = Some(WorktreeInfo {
                    branch: branch.clone(),
                    main_repo_path: main_repo_path.to_string_lossy().into_owned(),
                    managed_by_aoe: true,
                    created_at: Utc::now(),
                    base_branch: base,
                });
                instance.project_path.clone_from(&final_path);
                instance.worktree_info.clone_from(&worktree_info);
                creation_intent = Some(CreationIntent::reserve(storage, &mut instance)?);
                let w = git_wt.create_worktree_owned(
                    branch,
                    &worktree_path,
                    true,
                    worktree_info
                        .as_ref()
                        .and_then(|info| info.base_branch.as_deref()),
                    creation_intent.as_ref().unwrap(),
                )?;
                warnings.extend(w);
            }
        }
    }

    // For scratch sessions, `final_path` is intentionally empty here; the scratch directory is
    // provisioned below using the instance id allocated at admission.
    if !params.scratch {
        let final_path_buf = PathBuf::from(&final_path);
        if !final_path_buf.exists() {
            bail!("Project path does not exist: {}", final_path);
        }
        if !final_path_buf.is_dir() {
            bail!("Project path is not a directory: {}", final_path);
        }
    }

    instance.project_path = final_path;
    if params.scratch {
        instance.project_path = super::scratch::planned_scratch_path(&instance.id)?
            .to_string_lossy()
            .into_owned();
        instance.scratch = true;
        creation_intent = Some(CreationIntent::reserve(storage, &mut instance)?);
        creation_intent
            .as_ref()
            .unwrap()
            .provision_directory(std::path::Path::new(&instance.project_path))?;
    }
    instance.worktree_info = worktree_info;
    instance.workspace_info = workspace_info;
    let creation_intent = match creation_intent {
        Some(intent) => intent,
        None => CreationIntent::reserve_metadata(storage, &mut instance)?,
    };
    creation_intent.refresh_prepared(&instance)?;

    Ok(BuildResult {
        instance,
        warnings,
        creation_intent,
    })
}

/// Complete the original filesystem intent and return the actual metadata publication.
pub(crate) fn publish_prepared_creation_under_workspace_claim_lock<F>(
    storage: &super::Storage,
    prepared: &Instance,
    intent: &CreationIntent,
    update_groups: F,
) -> Result<Instance>
where
    F: FnOnce(&mut Vec<Instance>, &mut Vec<super::Group>) -> Result<()>,
{
    anyhow::ensure!(
        storage.same_origin_as(intent.storage()),
        "creation changed its physical owner"
    );
    intent.publish_under_workspace_claim_lock(prepared, |rows, groups, committed| {
        if super::is_duplicate_session(
            rows.iter(),
            &committed.title,
            &committed.project_path,
            Some(&committed.id),
        ) {
            return Err(super::duplicate_session_error(&committed.title));
        }
        let slot = rows
            .iter_mut()
            .find(|row| row.id == committed.id)
            .ok_or_else(|| anyhow::anyhow!("original creation row disappeared"))?;
        *slot = committed;
        update_groups(rows, groups)?;
        rows.iter()
            .find(|row| row.id == prepared.id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("publication removed its original creation row"))
    })
}

/// Hold the workspace and identity fences through creation publication.
pub(crate) struct CleanupOwnershipLocks {
    _workspace_claim: crate::session::StorageFlock,
    _identity: crate::session::StorageFlock,
}

impl CleanupOwnershipLocks {
    /// Acquire in the single order every other owner uses: workspace claim
    /// before identity. Failing to acquire is an error, never a partial hold,
    /// so a caller can fail closed and retain the failed create's resources.
    pub(crate) fn acquire() -> Result<Self> {
        let workspace_claim = crate::session::acquire_session_workspace_claim_lock()?;
        let identity = crate::session::acquire_session_identity_lock()?;
        Ok(Self {
            _workspace_claim: workspace_claim,
            _identity: identity,
        })
    }
    /// Reuse the publisher's workspace and identity fences.
    pub(crate) fn from_held(
        workspace_claim: crate::session::StorageFlock,
        identity: crate::session::StorageFlock,
    ) -> Self {
        Self {
            _workspace_claim: workspace_claim,
            _identity: identity,
        }
    }
}

/// Structured-view (ACP) helpers for the TUI create paths.
pub mod structured {
    use super::Instance;

    /// True when `tool` can back a structured-view session: it resolves in the ACP agent registry,
    /// the resolved config declares a parsable `[session.agent_acp_cmd]` command for it, or it is a
    /// custom agent that inherits a registry-backed base through `[session.agent_detect_as]` (e.g.
    /// a Claude wrapper that only overrides profile/oauth locations).
    pub fn tool_acp_capable(tool: &str, config: &crate::session::Config) -> bool {
        crate::acp::agent_registry::AgentRegistry::with_defaults()
            .get(tool)
            .is_some()
            || config
                .session
                .agent_acp_cmd
                .get(tool)
                .is_some_and(|cmd| crate::acp::AgentSpec::from_acp_cmd(tool, cmd).is_ok())
            || crate::acp::inherited_acp_base(tool, &config.session.agent_detect_as).is_some()
    }

    /// Pre-create validation for an explicit structured-view choice from the new-session wizard,
    /// run BEFORE any worktree / scratch / container is provisioned so a refusal can't orphan
    /// resources (same ordering as the CLI's precondition).
    pub fn validate_structured_choice(
        tool: &str,
        command_override: &str,
        config: &crate::session::Config,
    ) -> Result<(), String> {
        if !tool_acp_capable(tool, config) {
            return Err(format!(
                "tool `{tool}` is not ACP-capable: it has no agent registry entry and no \
                 [session.agent_acp_cmd] command. Run `aoe acp doctor` to see configured \
                 agents, or turn Structured off for a terminal session."
            ));
        }
        if !command_override.trim().is_empty() {
            return Ok(());
        }
        let registry = crate::acp::agent_registry::AgentRegistry::with_defaults();
        let spec = match registry.get(tool) {
            Some(spec) => spec.clone(),
            None => match config.session.agent_acp_cmd.get(tool) {
                Some(cmd) => crate::acp::AgentSpec::from_acp_cmd(tool, cmd)
                    .map_err(|e| format!("invalid [session.agent_acp_cmd] for `{tool}`: {e}"))?,
                // A custom agent that inherits a registry-backed base runs the
                // base agent's adapter, so the on-PATH check targets that.
                None => match crate::acp::inherited_acp_base(tool, &config.session.agent_detect_as)
                    .and_then(|base| registry.get(&base).cloned())
                {
                    Some(spec) => spec,
                    None => unreachable!("tool_acp_capable implies a resolvable spec"),
                },
            },
        };
        if !crate::cli::acp::command_present(&spec.command) {
            let hint = crate::acp::install_hints::install_hint_for(&spec.command)
                .unwrap_or("install via your package manager and retry");
            return Err(format!(
                "ACP adapter `{}` is not installed or not on $PATH. Install: {hint}. \
                 Or run `aoe acp doctor --fix`, or turn Structured off for a terminal session.",
                spec.command
            ));
        }
        Ok(())
    }

    /// Apply a validated structured-view choice to a freshly-built instance: set the persisted view
    /// and pin the per-agent default model, the same post-build step the web create handler runs.
    pub fn apply_structured_choice(instance: &mut Instance) {
        let config = crate::session::config::repo_config::resolve_config_with_repo_or_warn(
            &instance.source_profile,
            std::path::Path::new(&instance.project_path),
        );
        if !tool_acp_capable(&instance.tool, &config) {
            tracing::warn!(
                target: "session.create",
                session = %instance.id,
                tool = %instance.tool,
                "structured view requested for non-ACP tool; keeping terminal view"
            );
            return;
        }
        instance.view = crate::session::View::Structured;
        // Pin the per-agent default model so the composer shows it and the session stays on it
        // (mirrors the CLI and web create paths).
        let defaults = config.acp.acp_defaults_for(&instance.tool);
        instance.agent_model = crate::session::config::resolve_spawn_model_effort(
            defaults,
            instance.agent_model.take(),
            None,
        )
        .0;
    }
}

/// Resolve the session title: use the provided title, then an explicit worktree
/// branch name, then fall back to a random civilization name.
pub(crate) fn resolve_title(
    title: &str,
    worktree_branch: Option<&str>,
    worktree_enabled: bool,
    existing_titles: &[&str],
    taken_branches: &HashSet<String>,
) -> Result<String> {
    let taken_branch_keys = branch_collision_keys(taken_branches);
    let resolved = if title.is_empty() {
        if worktree_enabled {
            if let Some(branch) = worktree_branch.filter(|b| !b.trim().is_empty()) {
                branch.trim().to_string()
            } else {
                civilizations::generate_random_title_filtered(existing_titles, |candidate| {
                    branch_key_taken(&branch_name_from_title(candidate), &taken_branch_keys)
                })
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "Could not generate a unique worktree title or branch; please enter one manually."
                    )
                })?
            }
        } else {
            civilizations::generate_random_title(existing_titles)
        }
    } else {
        title.to_string()
    };

    Ok(resolved)
}

pub(crate) fn collect_taken_branches_for_derived_dedupe(
    existing_branches: &[&str],
    path: &str,
    extra_repo_paths: &[String],
    worktree_enabled: bool,
    create_new_branch: bool,
    scratch: bool,
) -> HashSet<String> {
    let mut taken: HashSet<String> = existing_branches.iter().map(|s| (*s).to_string()).collect();

    if worktree_enabled && create_new_branch && !scratch {
        for repo in std::iter::once(path)
            .chain(extra_repo_paths.iter().map(String::as_str))
            .filter(|s| !s.trim().is_empty())
        {
            if let Ok(local) = crate::git::diff::list_branches(std::path::Path::new(repo)) {
                taken.extend(local);
            }
        }
    }

    taken
}

/// Origin of an effective worktree branch name.
#[derive(Debug, Clone)]
pub(crate) enum BranchSource {
    /// User typed this name explicitly. Treat conflicts as a hard error.
    Explicit(String),
    /// Derived from the session title. Suffix on conflict.
    Derived(String),
}

fn resolve_worktree_branch(
    worktree_enabled: bool,
    worktree_branch: Option<&str>,
    final_title: &str,
) -> Option<BranchSource> {
    if !worktree_enabled {
        return None;
    }
    Some(
        match worktree_branch.map(str::trim).filter(|b| !b.is_empty()) {
            // Defense-in-depth: even if the frontend slug missed a forbidden char (or the caller is
            // a CLI/API user typing a title-shaped string into the branch field), sanitise here so
            // libgit2 never sees a value it'll reject with InvalidSpec.
            Some(b) => BranchSource::Explicit(git_sanitize_branch_name(b)),
            None => BranchSource::Derived(branch_name_from_title(final_title)),
        },
    )
}

/// Replace characters that git ref names cannot contain (per `git-check-ref-format(1)`) with '-'.
pub(crate) fn git_sanitize_branch_name(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_was_dash = false;
    for ch in s.trim().chars() {
        let forbidden = ch.is_whitespace()
            || ch.is_control()
            || matches!(ch, '~' | '^' | ':' | '?' | '*' | '[' | '\\');
        let push_ch = if forbidden { '-' } else { ch };
        if push_ch == '-' {
            if out.is_empty() || last_was_dash {
                continue;
            }
            last_was_dash = true;
        } else {
            last_was_dash = false;
        }
        out.push(push_ch);
    }
    // Disallowed multi-char sequences: ".." and "@{".
    let mut out = out.replace("..", "-").replace("@{", "-");
    // Strip the ".lock" suffix from every slash-separated component, not just the last one;
    // git-check-ref-format(1) rejects any component ending in ".lock" (e.g. `foo.lock/bar` is just
    // as invalid as `foo.lock`).
    out = out
        .split('/')
        .map(|mut seg| {
            while let Some(stripped) = seg.strip_suffix(".lock") {
                seg = stripped;
            }
            seg
        })
        .collect::<Vec<_>>()
        .join("/");
    while matches!(out.chars().last(), Some('-' | '.' | '/')) {
        out.pop();
    }
    while matches!(out.chars().next(), Some('-' | '.' | '/')) {
        out.remove(0);
    }
    // A lone '@' and the symbolic ref HEAD are also rejected by git as
    // complete ref names.
    if out.is_empty() || out == "@" || out == "HEAD" {
        "session".to_string()
    } else {
        out
    }
}

/// Find the next branch name not present in `taken`.
fn branch_collision_key(branch: &str) -> String {
    branch.to_ascii_lowercase()
}

fn branch_collision_keys(taken: &HashSet<String>) -> HashSet<String> {
    taken
        .iter()
        .map(|branch| branch_collision_key(branch))
        .collect()
}

fn branch_key_taken(branch: &str, taken_keys: &HashSet<String>) -> bool {
    taken_keys.contains(&branch_collision_key(branch))
}

fn dedupe_branch_name(base: &str, taken: &HashSet<String>) -> String {
    let taken_keys = branch_collision_keys(taken);
    if !branch_key_taken(base, &taken_keys) {
        return base.to_string();
    }
    let mut n = 2usize;
    loop {
        let candidate = format!("{}-{}", base, n);
        if !branch_key_taken(&candidate, &taken_keys) {
            return candidate;
        }
        n += 1;
    }
}

/// Map Latin ligatures and stroked letters to their conventional ASCII expansions.
fn expand_ligature(c: char) -> Option<&'static str> {
    Some(match c {
        'ß' => "ss",
        'æ' => "ae",
        'Æ' => "AE",
        'œ' => "oe",
        'Œ' => "OE",
        'ø' => "o",
        'Ø' => "O",
        'ł' => "l",
        'Ł' => "L",
        'đ' => "d",
        'Đ' => "D",
        'þ' => "th",
        'Þ' => "Th",
        _ => return None,
    })
}

pub(crate) fn branch_name_from_title(title: &str) -> String {
    use unicode_normalization::UnicodeNormalization;

    let mut branch = String::new();
    let mut last_was_dash = false;

    let mut push_processed = |ch: char| {
        // Preserve '/' as git's namespace separator (so a title like `jacob/feature-1` yields a
        // branch `jacob/feature-1`).
        if ch == '/' {
            while branch.ends_with('-') {
                branch.pop();
            }
            if branch.is_empty() || branch.ends_with('/') {
                return;
            }
            branch.push('/');
            last_was_dash = true;
            return;
        }

        let next = if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
            Some(ch.to_ascii_lowercase())
        } else if ch.is_whitespace() || ch.is_ascii_punctuation() {
            Some('-')
        } else {
            None
        };

        if let Some(ch) = next {
            if ch == '-' {
                if branch.is_empty() || last_was_dash {
                    return;
                }
                last_was_dash = true;
            } else {
                last_was_dash = false;
            }
            branch.push(ch);
        }
    };

    for ch in title.trim().nfkd() {
        match expand_ligature(ch) {
            Some(expansion) => expansion.chars().for_each(&mut push_processed),
            None => push_processed(ch),
        }
    }

    while branch.ends_with('-') || branch.ends_with('/') {
        branch.pop();
    }

    if branch.is_empty() {
        "session".to_string()
    } else {
        branch
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roman_for_test(n: u32) -> String {
        let mut remaining = n;
        let mut result = String::new();
        for (value, numeral) in [
            (1000, "M"),
            (900, "CM"),
            (500, "D"),
            (400, "CD"),
            (100, "C"),
            (90, "XC"),
            (50, "L"),
            (40, "XL"),
            (10, "X"),
            (9, "IX"),
            (5, "V"),
            (4, "IV"),
            (1, "I"),
        ] {
            while remaining >= value {
                result.push_str(numeral);
                remaining -= value;
            }
        }
        result
    }

    #[test]
    fn resolve_title_prefers_explicit_then_branch_then_civilization() {
        let taken = HashSet::new();
        assert_eq!(
            resolve_title("My Session", Some("feature-auth"), true, &[], &taken).unwrap(),
            "My Session"
        );
        assert_eq!(
            resolve_title("Custom Name", None, false, &[], &taken).unwrap(),
            "Custom Name"
        );
        assert_eq!(
            resolve_title("", Some("feature-auth"), true, &[], &taken).unwrap(),
            "feature-auth"
        );
        let generated = resolve_title("", None, false, &[], &taken).unwrap();
        assert!(
            civilizations::CIVILIZATIONS.contains(&generated.as_str()),
            "expected a civilization name, got: {generated}"
        );

        let existing: Vec<&str> = civilizations::CIVILIZATIONS
            .iter()
            .copied()
            .filter(|civ| *civ != "Tatars")
            .collect();
        let mut taken = HashSet::new();
        taken.insert("tatars".to_string());

        let title = resolve_title("", None, true, &existing, &taken).unwrap();

        assert_ne!(title, "Tatars");
        assert!(
            title.contains(" II"),
            "expected suffixed fallback after the only bare civ branch was taken, got: {title}"
        );
    }

    #[test]
    fn test_empty_worktree_title_errors_when_generation_exhausts() {
        let existing: Vec<&str> = civilizations::CIVILIZATIONS.to_vec();
        let mut taken = HashSet::new();

        for civ in civilizations::CIVILIZATIONS {
            for n in 2..=1000 {
                taken.insert(branch_name_from_title(&format!(
                    "{} {}",
                    civ,
                    roman_for_test(n)
                )));
            }
        }

        let timestamp = chrono::Utc::now().timestamp();
        for civ in civilizations::CIVILIZATIONS {
            for n in timestamp - 60..timestamp + 1060 {
                taken.insert(branch_name_from_title(&format!("{} {}", civ, n)));
            }
        }

        let err = resolve_title("", None, true, &existing, &taken).unwrap_err();

        assert!(
            err.to_string().contains("please enter one manually"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn resolve_worktree_branch_cases() {
        let branch = |name: Option<&str>| resolve_worktree_branch(true, name, "Fix Login Flow");
        assert!(matches!(branch(None), Some(BranchSource::Derived(s)) if s == "fix-login-flow"));
        assert!(
            matches!(branch(Some("feat/auth")), Some(BranchSource::Explicit(s)) if s == "feat/auth")
        );
        assert!(
            matches!(branch(Some("Exploration and issues v2")), Some(BranchSource::Explicit(s)) if s == "Exploration-and-issues-v2")
        );
        assert!(
            resolve_worktree_branch(false, Some("feat/auth"), "Fix Login Flow").is_none(),
            "no worktree means no branch to resolve"
        );
    }

    #[test]
    fn git_sanitize_branch_name_cases() {
        for (input, want) in [
            // Valid refs pass through untouched.
            ("feat/auth", "feat/auth"),
            ("release-1.2.3", "release-1.2.3"),
            ("user_name/topic", "user_name/topic"),
            // Characters git forbids in a ref.
            ("has spaces", "has-spaces"),
            ("a:b?c*d", "a-b-c-d"),
            ("ref^name", "ref-name"),
            ("a..b", "a-b"),
            ("a@{b", "a-b"),
            // Trimmed edges.
            ("  hello  ", "hello"),
            ("-leading", "leading"),
            (".hidden", "hidden"),
            ("/foo", "foo"),
            ("foo/", "foo"),
            // `.lock` is stripped per component, however many are stacked.
            ("foo.lock", "foo"),
            ("foo.lock/bar", "foo/bar"),
            ("feat/release.lock/v2", "feat/release/v2"),
            ("foo.lock.lock", "foo"),
            ("feat/release.lock.lock/v2.lock.lock", "feat/release/v2"),
            // Nothing usable, or a ref with a reserved meaning of its own.
            ("", "session"),
            ("@", "session"),
            ("HEAD", "session"),
        ] {
            assert_eq!(git_sanitize_branch_name(input), want, "input {input:?}");
        }
    }

    #[test]
    fn branch_name_from_title_cases() {
        for (title, want) in [
            // Git-hostile punctuation.
            ("Fix: login @ mobile #42", "fix-login-mobile-42"),
            ("feat/auth.refactor", "feat/auth-refactor"),
            // Slashes are kept as path separators but never doubled or dangling.
            ("jacob/feature-1", "jacob/feature-1"),
            ("/leading", "leading"),
            ("trailing/", "trailing"),
            ("a//b", "a/b"),
            ("a / b", "a/b"),
            // Latin diacritics and ligatures fold to ASCII.
            ("café fix", "cafe-fix"),
            ("naïve solution", "naive-solution"),
            ("Straße", "strasse"),
            ("Łódź", "lodz"),
            ("crème brûlée", "creme-brulee"),
            ("œuvre", "oeuvre"),
            // Scripts with no ASCII folding drop out.
            ("测试", "session"),
            ("🚀 ship", "ship"),
        ] {
            assert_eq!(branch_name_from_title(title), want, "title {title:?}");
        }
    }

    #[test]
    fn dedupe_branch_name_suffixes_past_every_taken_name() {
        let mut taken = HashSet::new();
        assert_eq!(dedupe_branch_name("fix-bug", &taken), "fix-bug");

        taken.insert("fix-bug".to_string());
        assert_eq!(dedupe_branch_name("fix-bug", &taken), "fix-bug-2");

        taken.extend(["fix-bug-2".to_string(), "fix-bug-3".to_string()]);
        assert_eq!(dedupe_branch_name("fix-bug", &taken), "fix-bug-4");

        taken.insert("Tatars".to_string());
        assert_eq!(
            dedupe_branch_name("tatars", &taken),
            "tatars-2",
            "collisions are case-insensitive"
        );
    }

    fn init_repo_with_commit(name: &str) -> tempfile::TempDir {
        let parent = tempfile::Builder::new()
            .prefix("aoe-test-")
            .tempdir()
            .unwrap();
        let dir = parent.path().join(name);
        std::fs::create_dir(&dir).unwrap();
        let repo = git2::Repository::init(&dir).unwrap();
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        std::fs::write(dir.join("README.md"), format!("{name}\n")).unwrap();
        let tree_id = {
            let mut index = repo.index().unwrap();
            index.add_path(std::path::Path::new("README.md")).unwrap();
            index.write_tree().unwrap()
        };
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();
        parent
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn hosted_git_creation(
        storage: &super::super::Storage,
        post_checkout: Option<&str>,
    ) -> (tempfile::TempDir, BuildResult) {
        let parent = init_repo_with_commit("hosted-repo");
        let repo = parent.path().join("hosted-repo").canonicalize().unwrap();
        if let Some(script) = post_checkout {
            use std::os::unix::fs::PermissionsExt;
            let hook = repo.join(".git/hooks/post-checkout");
            std::fs::write(&hook, format!("#!/bin/sh\n{script}\n")).unwrap();
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let mut params = custom_agent_params(&repo, "claude");
        params.worktree_enabled = true;
        params.worktree_branch = Some("hosted-native".into());
        params.create_new_branch = true;
        let build = build_instance(params, &[], &[], storage).unwrap();
        (parent, build)
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    #[ignore = "real native Creating proof, hosted Linux/macOS only"]
    #[serial_test::serial]
    fn hosted_creating_remote_branch_tracking_and_original_withdrawal() {
        crate::session::test_support::require_hosted_creating_native();
        let home = tempfile::tempdir().unwrap();
        let _home_guard = crate::session::test_support::isolate_home(home.path());
        let storage = std::sync::Arc::new(super::super::Storage::new_unwatched("default").unwrap());
        let parent = init_repo_with_commit("hosted-repo");
        let source = parent.path().join("hosted-repo").canonicalize().unwrap();
        let repository = git2::Repository::open(&source).unwrap();
        let oid = repository.head().unwrap().target().unwrap();
        let remote_path = parent.path().join("remote.git");
        let remote = git2::build::RepoBuilder::new()
            .bare(true)
            .clone(source.to_str().unwrap(), &remote_path)
            .unwrap();
        remote
            .branch("hosted-existing", &remote.find_commit(oid).unwrap(), false)
            .unwrap();
        repository
            .remote("origin", remote_path.to_str().unwrap())
            .unwrap();
        repository
            .config()
            .unwrap()
            .set_str("user.fixture", "keep")
            .unwrap();
        assert!(repository
            .find_branch("hosted-existing", git2::BranchType::Local)
            .is_err());
        let mut params = custom_agent_params(&source, "claude");
        params.worktree_enabled = true;
        params.worktree_branch = Some("hosted-existing".into());
        params.create_new_branch = false;
        let build = build_instance(params, &[], &[], &storage).unwrap();
        let prepared = build.instance;
        let checkout = PathBuf::from(&prepared.project_path);
        assert_eq!(
            git2::Repository::open(&checkout)
                .unwrap()
                .head()
                .unwrap()
                .target(),
            Some(oid)
        );
        assert_eq!(
            std::fs::read(checkout.join("README.md")).unwrap(),
            b"hosted-repo
"
        );
        let config = repository.config().unwrap();
        assert_eq!(
            config.get_string("branch.hosted-existing.remote").unwrap(),
            "origin"
        );
        assert_eq!(
            config.get_string("branch.hosted-existing.merge").unwrap(),
            "refs/heads/hosted-existing"
        );
        let canonical = storage.load().unwrap().pop().unwrap();
        hosted_creation_receipts(&canonical);
        let custody = CreationCustody::retained()
            .into_iter()
            .find(|original| original.session_id() == prepared.id)
            .unwrap();
        #[cfg(target_os = "linux")]
        {
            let ack = custody.withdraw().unwrap();
            assert!(ack
                .matches_original(
                    &storage,
                    &prepared.id,
                    prepared.created_at,
                    prepared.lifecycle_generation
                )
                .unwrap());
            assert!(!checkout.exists());
            assert!(repository
                .find_branch("hosted-existing", git2::BranchType::Local)
                .is_err());
            let config = repository.config().unwrap();
            assert!(config.get_string("branch.hosted-existing.remote").is_err());
            assert!(config.get_string("branch.hosted-existing.merge").is_err());
            assert_eq!(config.get_string("user.fixture").unwrap(), "keep");
            assert_eq!(
                repository
                    .find_reference("refs/remotes/origin/hosted-existing")
                    .unwrap()
                    .target(),
                Some(oid)
            );
            assert!(storage.load().unwrap().is_empty());
        }
        #[cfg(target_os = "macos")]
        {
            assert!(custody.withdraw().is_err());
            let retained = storage.load().unwrap().pop().unwrap();
            assert_eq!(
                retained.lifecycle_reservation,
                canonical.lifecycle_reservation
            );
            assert_eq!(
                retained.lifecycle_generation,
                canonical.lifecycle_generation
            );
            assert!(checkout.is_dir());
        }
        assert_eq!(repository.head().unwrap().target(), Some(oid));
        println!("hosted remote Creating: actual fetch, original branch/config issuances, checkout and truthful original withdrawal outcome");
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn hosted_creation_receipts(row: &Instance) -> serde_json::Value {
        let journal = serde_json::to_value(&row.runner_journal).unwrap();
        let records = journal["creations"]
            .as_array()
            .expect("native creation journal");
        assert!(
            !records.is_empty(),
            "a real native effect must publish receipts"
        );
        for record in records {
            assert_eq!(record["session_id"], row.id);
            assert_eq!(record["generation"], row.lifecycle_generation);
            assert!(record["births"]
                .as_array()
                .is_some_and(|births| !births.is_empty()));
            assert!(
                record["root_status"].is_number(),
                "actual producer must acknowledge exit"
            );
            assert_eq!(
                record["effect_acknowledged"], true,
                "publication needs the actual producer CAS ACK"
            );
            assert_eq!(record["scope_unproven"], cfg!(target_os = "macos"));
        }
        serde_json::json!({"create_coverage": journal["create_coverage"], "creations": records})
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "real native Git attach proof, hosted Linux only"]
    #[serial_test::serial]
    fn hosted_creating_normal_git_is_quiescent_before_unstarted_attach() {
        crate::session::test_support::require_hosted_creating_native();
        let home = tempfile::tempdir().unwrap();
        let _home_guard = crate::session::test_support::isolate_home(home.path());
        let storage = std::sync::Arc::new(super::super::Storage::new_unwatched("default").unwrap());
        let (_parent, build) = hosted_git_creation(&storage, None);
        let prepared = build.instance;
        let canonical = storage.load().unwrap().pop().unwrap();
        let original_journal = serde_json::to_value(&canonical.runner_journal).unwrap();
        let records = original_journal["creations"].as_array().unwrap();
        for record in records {
            assert_eq!(record["session_id"], prepared.id);
            assert_eq!(
                record["created_at"],
                serde_json::to_value(prepared.created_at).unwrap()
            );
            assert_eq!(record["generation"], canonical.lifecycle_generation);
            assert!(record["root_status"].is_number());
            assert_eq!(record["effect_acknowledged"], true);
        }
        println!("normal Git original journal before publication: {original_journal}");
        let custody = CreationCustody::retained()
            .into_iter()
            .find(|original| original.session_id() == prepared.id)
            .unwrap();
        custody
            .retain_ready(CreationReady {
                instance: prepared,
                warnings: build.warnings,
                on_launch_hooks_ran: false,
            })
            .unwrap();
        let published = custody.retry_publication().unwrap();
        let published_journal = serde_json::to_value(&published.runner_journal).unwrap();
        assert_eq!(published_journal, original_journal);
        assert!(published_journal["launches"].as_array().unwrap().is_empty());
        assert!(published_journal["preparations"]
            .as_array()
            .unwrap()
            .is_empty());
        assert!(
            published.runner_journal.proves_runner_quiescent(),
            "unstarted runner proof: {published_journal}"
        );
        assert!(published.runner_journal.proves_quiescent(), "normal Git Create prevents unstarted attach; actual rejecting bits: {published_journal}");

        let frontend_parent = init_repo_with_commit("frontend");
        let frontend = frontend_parent
            .path()
            .join("frontend")
            .canonicalize()
            .unwrap();
        let mut plan = super::super::attach_project::plan(
            &published,
            "default",
            &frontend,
            super::super::attach_project::ExistingBranch::Refuse,
        )
        .unwrap();
        let acknowledged =
            super::super::attach_project::reserve_attach(&storage, &published.id, &mut plan)
                .unwrap();
        let outcome = super::super::attach_project::quiesce_for_conversion(
            &storage,
            &published,
            &plan,
            acknowledged,
        );
        if let Err(error) = outcome {
            panic!("normal Git original attach scope refused: {error:#}; published producer journal: {published_journal}");
        }
        super::super::attach_project::release_attach(&plan);
        let retained = storage.load().unwrap().pop().unwrap();
        assert_eq!(retained.project_path, published.project_path);
        assert_eq!(retained.worktree_info, published.worktree_info);
        assert_eq!(
            serde_json::to_value(&retained.runner_journal).unwrap(),
            original_journal
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "real native Creating proof, hosted Linux/macOS only"]
    #[serial_test::serial]
    async fn hosted_creating_checkout_publication_and_managed_launch_preserve_receipts() {
        crate::session::test_support::require_hosted_creating_native();
        let home = tempfile::tempdir().unwrap();
        let _home_guard = crate::session::test_support::isolate_home(home.path());
        let storage = std::sync::Arc::new(super::super::Storage::new_unwatched("default").unwrap());
        let (_parent, build) = hosted_git_creation(&storage, None);
        let mut prepared = build.instance;
        prepared.view = crate::session::View::Structured;
        build.creation_intent.refresh_prepared(&prepared).unwrap();
        let checkout = PathBuf::from(&prepared.project_path);
        assert_eq!(
            std::fs::read(checkout.join("README.md")).unwrap(),
            b"hosted-repo\n"
        );
        assert_eq!(
            git2::Repository::open(&checkout)
                .unwrap()
                .head()
                .unwrap()
                .shorthand()
                .unwrap(),
            "hosted-native"
        );
        let canonical = storage.load().unwrap().pop().unwrap();
        let generation = canonical.lifecycle_generation;
        let receipts = hosted_creation_receipts(&canonical);
        assert!(canonical.has_pending_worktree_path_claims());
        let custody = CreationCustody::retained()
            .into_iter()
            .find(|original| original.session_id() == prepared.id)
            .unwrap();
        custody
            .retain_ready(CreationReady {
                instance: prepared,
                warnings: build.warnings,
                on_launch_hooks_ran: false,
            })
            .unwrap();
        let published = custody.retry_publication().unwrap();
        assert_eq!(published.lifecycle_generation, generation);
        assert!(published.lifecycle_reservation.is_none());
        assert_eq!(hosted_creation_receipts(&published), receipts);
        assert!(custody
            .matches_original(&storage, &published.id, published.created_at, generation)
            .unwrap());
        let execution = crate::acp::supervisor::test_support::published_execution(
            &published.id,
            "default",
            None,
            false,
        );
        assert!(execution.identity.birth_is_complete());
        assert_eq!(execution.identity.generation, generation + 1);
        let launched = storage.load().unwrap().pop().unwrap();
        let journal = serde_json::to_value(&launched.runner_journal).unwrap();
        assert_eq!(journal["creations"], receipts["creations"]);
        assert_eq!(journal["create_coverage"], receipts["create_coverage"]);
        assert_eq!(launched.lifecycle_generation, generation + 1);
        assert!(!launched.runner_journal.proves_quiescent());
        assert!(custody
            .matches_original(&storage, &published.id, published.created_at, generation)
            .unwrap());
        assert_eq!(
            std::fs::read(checkout.join("README.md")).unwrap(),
            b"hosted-repo\n"
        );
        println!("hosted Creating: genuine checkout, same-g publication, actual ManagedLaunch g+1; original receipts unchanged");
        drop(execution);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    #[ignore = "real native Creating proof, hosted Linux/macOS only"]
    #[serial_test::serial]
    fn hosted_creating_hook_exit23_retains_error_and_original_withdrawal_outcome() {
        crate::session::test_support::require_hosted_creating_native();
        let home = tempfile::tempdir().unwrap();
        let _home_guard = crate::session::test_support::isolate_home(home.path());
        let storage = std::sync::Arc::new(super::super::Storage::new_unwatched("default").unwrap());
        let (parent, build) = hosted_git_creation(&storage, Some("exit 23"));
        assert!(
            build.warnings.iter().any(|warning| warning.contains("23")),
            "post-checkout exit23 must remain visible"
        );
        let prepared = build.instance;
        let future = PathBuf::from(&prepared.project_path);
        let custody = CreationCustody::retained()
            .into_iter()
            .find(|original| original.session_id() == prepared.id)
            .unwrap();
        let intent = build.creation_intent;
        let generation = prepared.lifecycle_generation;
        let claim = prepared.lifecycle_reservation.clone();
        let peer = parent.path().join("preexisting-peer");
        std::fs::create_dir(&peer).unwrap();
        std::fs::write(peer.join("user-data"), b"keep").unwrap();
        let checkout = git2::Repository::open(&future).unwrap();
        let admin = checkout.path().canonicalize().unwrap();
        let common = parent
            .path()
            .join("hosted-repo/.git")
            .canonicalize()
            .unwrap();
        for path in [&future, &admin, &common] {
            assert!(super::super::AnchoredDir::open(path)
                .unwrap()
                .birth_identity()
                .unwrap()
                .is_durable());
        }
        drop(checkout);
        let (output_tx, output_rx) = std::sync::mpsc::channel();
        drop(output_rx);
        let error = crate::session::config::repo_config::execute_creating_hooks(
            &intent,
            &["printf 'hook-visible-output\\n'; exit 23".into()],
            &future,
            Some(&output_tx),
            &[],
            None,
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("23"),
            "original exit23 must not become success"
        );
        let original = storage.load().unwrap().pop().unwrap();
        let receipts = hosted_creation_receipts(&original);
        assert_eq!(
            receipts["creations"].as_array().unwrap().last().unwrap()["root_status"],
            23 << 8
        );
        let error = finish_failed_creation(&storage, &prepared, error);
        assert!(format!("{error:#}").contains("23"));
        #[cfg(target_os = "linux")]
        {
            assert_eq!(custody.undo_verdict(), CreationUndoVerdict::Withdrawn);
            assert!(!future.exists());
            assert!(!admin.exists());
            assert!(common.is_dir());
            assert!(storage.load().unwrap().is_empty());
            let ack = custody.withdraw().unwrap();
            assert_eq!(ack.generation(), generation);
            assert!(ack
                .matches_original(&storage, &prepared.id, prepared.created_at, generation)
                .unwrap());
            println!("hosted Linux Creating: exit23 preserved, actual tracked retirement and original same-g withdrawal");
        }
        #[cfg(target_os = "macos")]
        {
            assert!(
                custody.withdraw().is_err(),
                "uncertain Darwin scope cannot prove strong withdrawal"
            );
            let retained = storage.load().unwrap().pop().unwrap();
            assert_eq!(retained.lifecycle_generation, generation);
            assert_eq!(retained.lifecycle_reservation, claim);
            assert!(retained.has_pending_worktree_path_claims());
            assert!(future.is_dir());
            println!("hosted Darwin Creating: exit23 preserved; uncertain descendant scope retains original claim and resource");
        }
        assert_eq!(std::fs::read(peer.join("user-data")).unwrap(), b"keep");
        #[cfg(target_os = "linux")]
        let _ = claim;
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "real native Creating proof, hosted Linux only"]
    #[serial_test::serial]
    fn hosted_creating_branch_unlink_ack_survives_late_retirement_failure() {
        crate::session::test_support::require_hosted_creating_native();
        let home = tempfile::tempdir().unwrap();
        let _home_guard = crate::session::test_support::isolate_home(home.path());
        let storage = std::sync::Arc::new(super::super::Storage::new_unwatched("default").unwrap());
        let (parent, build) = hosted_git_creation(&storage, None);
        let prepared = build.instance;
        let source = git2::Repository::open(parent.path().join("hosted-repo")).unwrap();
        let original_head = source.head().unwrap().target();
        let custody = CreationCustody::retained()
            .into_iter()
            .find(|original| original.session_id() == prepared.id)
            .unwrap();
        struct ResetBranchFailure;
        impl Drop for ResetBranchFailure {
            fn drop(&mut self) {
                super::super::creation_undo::FAIL_BRANCH_RETIRE_ONCE.set(false);
            }
        }
        let _reset = ResetBranchFailure;
        super::super::creation_undo::FAIL_BRANCH_RETIRE_ONCE.set(true);
        let first = match custody.withdraw() {
            Err(error) => error,
            Ok(_) => panic!("expected late retirement failure"),
        };
        assert!(format!("{first:#}").contains("injected original branch retirement failure"));
        assert!(source
            .find_branch("hosted-native", git2::BranchType::Local)
            .is_err());
        assert!(!std::path::Path::new(&prepared.project_path).exists());
        let retained = storage.load().unwrap().pop().unwrap();
        assert_eq!(retained.lifecycle_generation, prepared.lifecycle_generation);
        assert_eq!(
            retained.lifecycle_reservation,
            prepared.lifecycle_reservation
        );
        hosted_creation_receipts(&retained);
        let ack = custody.withdraw().unwrap();
        assert!(ack
            .matches_original(
                &storage,
                &prepared.id,
                prepared.created_at,
                prepared.lifecycle_generation
            )
            .unwrap());
        assert!(storage.load().unwrap().is_empty());
        assert_eq!(source.head().unwrap().target(), original_head);
        assert!(source
            .find_branch("hosted-native", git2::BranchType::Local)
            .is_err());
        println!("hosted original branch Undo: real CAS delete ACK survived later failure; same-g retry completed without replaying deletion");
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    #[ignore = "real native Creating proof, hosted Linux/macOS only"]
    #[serial_test::serial]
    fn hosted_creating_changed_resources_refuse_deletion_and_keep_claims() {
        crate::session::test_support::require_hosted_creating_native();
        for change in ["dirty", "rewritten-file", "root", "admin", "common"] {
            let home = tempfile::tempdir().unwrap();
            let _home_guard = crate::session::test_support::isolate_home(home.path());
            let storage =
                std::sync::Arc::new(super::super::Storage::new_unwatched("default").unwrap());
            let (parent, build) = hosted_git_creation(&storage, None);
            let prepared = build.instance;
            let root = PathBuf::from(&prepared.project_path);
            let admin = git2::Repository::open(&root)
                .unwrap()
                .path()
                .canonicalize()
                .unwrap();
            let common = parent
                .path()
                .join("hosted-repo/.git")
                .canonicalize()
                .unwrap();
            let original_row = storage.load().unwrap().pop().unwrap();
            let receipts = hosted_creation_receipts(&original_row);
            let preserved = match change {
                "dirty" => {
                    std::fs::write(root.join("user-data"), b"keep").unwrap();
                    root.join("user-data")
                }
                "rewritten-file" => {
                    let file = root.join("README.md");
                    let bytes = std::fs::read(&file).unwrap();
                    std::fs::rename(&file, root.join("original-readme")).unwrap();
                    std::fs::write(&file, bytes).unwrap();
                    file
                }
                _ => {
                    let changed = match change {
                        "root" => &root,
                        "admin" => &admin,
                        "common" => &common,
                        _ => unreachable!(),
                    };
                    std::fs::rename(
                        changed,
                        changed.with_file_name(format!("retained-{change}")),
                    )
                    .unwrap();
                    std::fs::create_dir(changed).unwrap();
                    std::fs::write(changed.join("user-data"), b"keep").unwrap();
                    changed.join("user-data")
                }
            };
            let bytes = std::fs::read(&preserved).unwrap();
            let custody = CreationCustody::retained()
                .into_iter()
                .find(|original| original.session_id() == prepared.id)
                .unwrap();
            assert!(
                custody.withdraw().is_err(),
                "{change}: changed original must refuse deletion"
            );
            assert_eq!(
                std::fs::read(&preserved).unwrap(),
                bytes,
                "{change}: user bytes changed"
            );
            let retained = storage.load().unwrap().pop().unwrap();
            assert_eq!(
                retained.lifecycle_generation,
                original_row.lifecycle_generation
            );
            assert_eq!(
                retained.lifecycle_reservation,
                original_row.lifecycle_reservation
            );
            assert_eq!(retained.created_at, original_row.created_at);
            assert!(retained.has_pending_worktree_path_claims());
            let journal = serde_json::to_value(&retained.runner_journal).unwrap();
            assert_eq!(journal["creations"], receipts["creations"]);
            assert_eq!(journal["create_coverage"], receipts["create_coverage"]);
            assert!(root.is_dir());
            println!("hosted Creating: {change} preserved; original same-g claim retained");
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    #[ignore = "real native Creating proof, hosted Linux/macOS only"]
    #[serial_test::serial]
    fn hosted_creating_preexisting_checkout_is_never_undone() {
        crate::session::test_support::require_hosted_creating_native();
        let home = tempfile::tempdir().unwrap();
        let _home_guard = crate::session::test_support::isolate_home(home.path());
        let storage = std::sync::Arc::new(super::super::Storage::new_unwatched("default").unwrap());
        let (parent, build) = hosted_git_creation(&storage, None);
        let owner = build.creation_intent.publish(&build.instance).unwrap();
        let root = PathBuf::from(&owner.project_path);
        std::fs::write(root.join("user-data"), b"keep").unwrap();
        let repo = parent.path().join("hosted-repo").canonicalize().unwrap();
        let mut params = custom_agent_params(&repo, "claude");
        params.worktree_enabled = true;
        params.worktree_branch = Some("hosted-native".into());
        let second = build_instance(params, &[], &[], &storage).unwrap();
        assert!(
            !second
                .instance
                .worktree_info
                .as_ref()
                .unwrap()
                .managed_by_aoe
        );
        let generation = second.instance.lifecycle_generation;
        let custody = CreationCustody::retained()
            .into_iter()
            .find(|original| original.session_id() == second.instance.id)
            .unwrap();
        let error = crate::session::config::repo_config::execute_creating_hooks(
            &second.creation_intent,
            &["exit 23".into()],
            &root,
            None,
            &[],
            None,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("23"));
        #[cfg(target_os = "linux")]
        assert_eq!(custody.withdraw().unwrap().generation(), generation);
        #[cfg(target_os = "macos")]
        {
            assert!(custody.withdraw().is_err());
            let rows = storage.load().unwrap();
            let retained = rows
                .iter()
                .find(|row| row.id == second.instance.id)
                .unwrap();
            assert_eq!(retained.lifecycle_generation, generation);
            assert_eq!(
                retained.lifecycle_reservation,
                second.instance.lifecycle_reservation
            );
        }
        assert_eq!(std::fs::read(root.join("user-data")).unwrap(), b"keep");
        assert_eq!(
            std::fs::read(root.join("README.md")).unwrap(),
            b"hosted-repo\n"
        );
        assert!(git2::Repository::open(&root).unwrap().path().is_dir());
        assert!(storage.load().unwrap().iter().any(|row| row.id == owner.id));
        println!("hosted Creating: dirty preexisting checkout and canonical owner preserved");
    }

    #[test]
    #[serial_test::serial]
    fn original_creation_custody_survives_dropped_handles_and_retries_publication() {
        let home = tempfile::tempdir().unwrap();
        let _home_guard = crate::session::test_support::isolate_home(home.path());
        let storage = std::sync::Arc::new(super::super::Storage::new_unwatched("default").unwrap());
        let mut prepared = Instance::new("retained original", home.path().to_str().unwrap());
        let custody = CreationCustody::register(storage.clone(), &prepared).unwrap();
        let weak = std::sync::Arc::downgrade(&custody);
        let intent = CreationIntent::reserve_metadata(&storage, &mut prepared).unwrap();
        let generation = prepared.lifecycle_generation;
        drop(intent);
        drop(custody);
        let custody = weak
            .upgrade()
            .expect("process owner retains the actual original");
        assert!(custody
            .matches_original(&storage, &prepared.id, prepared.created_at, generation)
            .unwrap());
        assert!(!custody
            .matches_original(&storage, &prepared.id, prepared.created_at, generation + 1)
            .unwrap());
        assert!(custody.retry_publication().is_err());
        assert!(CreationIntent::reserve_metadata(&storage, &mut prepared).is_err());
        custody
            .retain_ready(CreationReady {
                instance: prepared,
                warnings: vec!["original warning".into()],
                on_launch_hooks_ran: false,
            })
            .unwrap();
        let published = custody.retry_publication().unwrap();
        assert_eq!(published.lifecycle_generation, generation);
        assert!(published.lifecycle_reservation.is_none());
        assert_eq!(custody.retry_publication().unwrap().id, published.id);
        assert_eq!(custody.ready().unwrap().warnings, vec!["original warning"]);
        assert_eq!(
            custody.undo_verdict(),
            CreationUndoVerdict::AlreadyPublished
        );
    }

    #[test]
    #[serial_test::serial]
    fn original_scratch_withdrawal_returns_producer_ack_without_renewing_create() {
        let home = tempfile::tempdir().unwrap();
        let _home_guard = crate::session::test_support::isolate_home(home.path());
        let storage = std::sync::Arc::new(super::super::Storage::new_unwatched("default").unwrap());
        let future = home.path().join("owned-scratch");
        let mut prepared = Instance::new("owned scratch", future.to_str().unwrap());
        prepared.scratch = true;
        let custody = CreationCustody::register(storage.clone(), &prepared).unwrap();
        let intent = CreationIntent::reserve(&storage, &mut prepared).unwrap();
        let generation = prepared.lifecycle_generation;
        intent.provision_directory(&future).unwrap();
        let ack = custody.withdraw().unwrap();
        assert!(ack
            .matches_original(&storage, &prepared.id, prepared.created_at, generation)
            .unwrap());
        assert!(!future.exists());
        assert!(!storage
            .load()
            .unwrap()
            .iter()
            .any(|row| row.id == prepared.id));
        assert_eq!(custody.withdraw().unwrap().generation(), generation);
    }

    #[test]
    #[serial_test::serial]
    fn original_checkout_undo_retries_after_git_unlink_sync_failure() {
        let home = tempfile::tempdir().unwrap();
        let _home_guard = crate::session::test_support::isolate_home(home.path());
        let source = home.path().join("source");
        let repository = git2::Repository::init(&source).unwrap();
        let tree_oid = repository.index().unwrap().write_tree().unwrap();
        let tree = repository.find_tree(tree_oid).unwrap();
        let signature = git2::Signature::now("fixture", "fixture@example.invalid").unwrap();
        let oid = repository
            .commit(Some("HEAD"), &signature, &signature, "original", &tree, &[])
            .unwrap();
        let commit = repository.find_commit(oid).unwrap();
        repository.branch("retained", &commit, false).unwrap();
        let storage = super::super::Storage::new_unwatched("default").unwrap();
        let checkout = home.path().join("owned-checkout");
        let mut prepared = Instance::new("owned checkout", checkout.to_str().unwrap());
        prepared.worktree_info = Some(super::super::WorktreeInfo {
            branch: "retained".into(),
            main_repo_path: source.to_str().unwrap().into(),
            managed_by_aoe: true,
            created_at: Utc::now(),
            base_branch: None,
        });
        let intent = CreationIntent::reserve(&storage, &mut prepared).unwrap();
        let reservation = prepared.lifecycle_reservation.clone();
        intent
            .allocate_worktree_bootstrap(&source, "retained", &checkout, "fixture")
            .unwrap();
        struct ResetSyncFailure;
        impl Drop for ResetSyncFailure {
            fn drop(&mut self) {
                super::super::anchored_fs::FAIL_SYNC_IDENTITY_ONCE.set(None);
            }
        }
        let _reset = ResetSyncFailure;
        let identity = super::super::AnchoredDir::open(&checkout)
            .unwrap()
            .identity()
            .unwrap();
        let admin = std::fs::read_dir(repository.path().join("worktrees"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        super::super::anchored_fs::FAIL_SYNC_IDENTITY_ONCE.set(Some(identity));
        let first = intent.undo_original().unwrap_err();
        assert!(format!("{first:#}").contains("injected anchored directory sync failure"));
        let staged = std::fs::read_dir(home.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                super::super::AnchoredDir::open(path)
                    .is_ok_and(|directory| directory.identity().is_ok_and(|seen| seen == identity))
            })
            .unwrap();
        assert!(staged.is_dir());
        assert!(!staged.join(".git").exists());
        assert_eq!(
            storage.load().unwrap()[0].lifecycle_reservation,
            reservation
        );
        intent.undo_original().unwrap();
        assert!(!staged.exists());
        assert!(!checkout.exists());
        assert!(!admin.exists());
        assert_eq!(
            repository
                .find_branch("retained", git2::BranchType::Local)
                .unwrap()
                .get()
                .target(),
            Some(oid)
        );
        assert_eq!(repository.head().unwrap().target(), Some(oid));
        assert!(storage
            .load()
            .unwrap()
            .iter()
            .all(|row| row.id != prepared.id));
    }

    #[test]
    #[serial_test::serial]
    fn dirty_original_scratch_retains_both_data_and_same_create_claim() {
        let home = tempfile::tempdir().unwrap();
        let _home_guard = crate::session::test_support::isolate_home(home.path());
        let storage = std::sync::Arc::new(super::super::Storage::new_unwatched("default").unwrap());
        let future = home.path().join("dirty-scratch");
        let mut prepared = Instance::new("dirty scratch", future.to_str().unwrap());
        prepared.scratch = true;
        let custody = CreationCustody::register(storage.clone(), &prepared).unwrap();
        let intent = CreationIntent::reserve(&storage, &mut prepared).unwrap();
        let generation = prepared.lifecycle_generation;
        intent.provision_directory(&future).unwrap();
        std::fs::write(future.join("user-data"), b"keep").unwrap();
        assert!(custody.withdraw().is_err());
        assert_eq!(std::fs::read(future.join("user-data")).unwrap(), b"keep");
        let stored = storage
            .load()
            .unwrap()
            .into_iter()
            .find(|row| row.id == prepared.id)
            .unwrap();
        assert_eq!(stored.lifecycle_generation, generation);
        assert_eq!(stored.lifecycle_reservation, prepared.lifecycle_reservation);
        assert!(custody
            .matches_original(&storage, &prepared.id, prepared.created_at, generation)
            .unwrap());
    }

    #[test]
    #[serial_test::serial]
    fn a_creation_intent_blocks_peer_paths_until_its_original_publication() {
        let home = tempfile::tempdir().unwrap();
        let _home_guard = crate::session::test_support::isolate_home(home.path());
        let storage = super::super::Storage::new_unwatched("default").unwrap();
        let peer = super::super::Storage::new_unwatched("peer").unwrap();
        std::fs::write(peer.sessions_path(), b"[]").unwrap();
        let future = home.path().join("future/worktree");
        let mut prepared = Instance::new("owned creation", future.to_str().unwrap());
        let intent = CreationIntent::reserve(&storage, &mut prepared).unwrap();
        assert!(!future.exists());
        let mut changed_plan = prepared.clone();
        changed_plan.project_path = home.path().join("another").to_string_lossy().into_owned();
        assert!(intent.publish(&changed_plan).is_err());
        assert!(storage
            .update(|rows, _| {
                rows[0].lifecycle_reservation = None;
                Ok(())
            })
            .is_err());
        let mut changed_origin = prepared.clone();
        changed_origin.storage_origin = Some(std::sync::Arc::new(peer.clone()));
        assert!(intent.publish(&changed_origin).is_err());
        let intruder = Instance::new(
            "overlapping peer",
            future.parent().unwrap().to_str().unwrap(),
        );
        assert!(peer
            .update(|rows, _| {
                rows.push(intruder.clone());
                Ok(())
            })
            .is_err());
        assert!(peer.load().unwrap().is_empty());
        assert!(storage.load().unwrap()[0].has_pending_worktree_path_claims());
        {
            let _workspace = super::super::acquire_session_workspace_claim_lock().unwrap();
            let claims = super::super::deletion::PathClaimIndex::load_for_writer(
                std::slice::from_ref(&peer),
            )
            .unwrap();
            assert!(claims.ensure_writes_unclaimed(&[future.as_path()]).is_err());
            assert!(claims
                .ensure_pending_writes_unclaimed(&[future.parent().unwrap()])
                .is_err());
            assert!(claims
                .ensure_pending_writes_unclaimed(&[future.join("child").as_path()])
                .is_err());
            assert!(claims
                .ensure_writes_unclaimed(&[home.path().join("unrelated").as_path()])
                .is_ok());
        }
        std::fs::create_dir_all(&future).unwrap();
        let peer_bytes = std::fs::read(peer.sessions_path()).unwrap();
        std::fs::write(peer.sessions_path(), b"broken ownership inventory").unwrap();
        assert!(intent.publish(&prepared).is_err());
        assert!(storage.load().unwrap()[0].has_pending_worktree_path_claims());
        std::fs::write(peer.sessions_path(), peer_bytes).unwrap();
        let committed = intent.publish(&prepared).unwrap();
        assert!(committed.lifecycle_reservation.is_none());
        let canonical = storage.load().unwrap().pop().unwrap();
        assert_eq!(canonical.project_path, future.to_str().unwrap());
        assert!(canonical.lifecycle_reservation.is_none());
        peer.update(|rows, _| {
            rows.push(intruder);
            Ok(())
        })
        .unwrap();
        assert_eq!(
            peer.load().unwrap()[0].project_path,
            future.parent().unwrap().to_str().unwrap()
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_create_workspace_reports_all_concurrent_failures() {
        let home = tempfile::tempdir().unwrap();
        let _home_guard = crate::session::test_support::isolate_home(home.path());
        let storage = super::super::Storage::new_unwatched("default").unwrap();
        let parent_a = init_repo_with_commit("repo-a-fail");
        let parent_b = init_repo_with_commit("repo-b-fail");
        let repo_a = parent_a.path().join("repo-a-fail");
        let repo_b = parent_b.path().join("repo-b-fail");
        let workspaces_root = tempfile::TempDir::new().unwrap();
        let template = workspaces_root
            .path()
            .join("{branch}")
            .to_string_lossy()
            .into_owned();
        let mut prepared = Instance::new("failed workspace", repo_a.to_str().unwrap());
        prepared.storage_origin = Some(std::sync::Arc::new(storage.clone()));

        let result = create_workspace(
            &WorkspaceRepoSpec {
                path: repo_a,
                base_branch: None,
            },
            &[WorkspaceRepoSpec {
                path: repo_b,
                base_branch: None,
            }],
            "nonexistent-branch",
            false,
            &template,
            true,
            &mut prepared,
        );

        let err = match result {
            Ok(_) => panic!("workspace creation should fail when no repo has the branch"),
            Err(e) => e,
        };
        let msg = format!("{err:#}");
        assert!(
            msg.contains("repo-a-fail"),
            "first repo name missing from message: {msg}"
        );
        assert!(
            msg.contains("repo-b-fail"),
            "second repo name missing from message: {msg}"
        );
        let stored = storage
            .load()
            .unwrap()
            .into_iter()
            .find(|row| row.id == prepared.id);
        #[cfg(target_os = "linux")]
        {
            assert!(
                stored.is_none(),
                "acknowledged withdrawal removes the original row"
            );
            assert!(!workspaces_root.path().join("nonexistent-branch").exists());
        }
        #[cfg(not(target_os = "linux"))]
        {
            let stored = stored.expect("uncertain native scope retains its original row");
            assert!(stored.has_pending_worktree_path_claims());
            assert!(matches!(stored.lifecycle_reservation.unwrap().path_claims,
                super::super::WorktreePathClaims::Pending(paths) if paths.contains(&workspaces_root.path().join("nonexistent-branch"))));
        }
    }

    #[test]
    fn resolve_base_branch_precedence() {
        assert_eq!(
            resolve_base_branch(Some("session"), Some("project"), Some("global")),
            Some("session".to_string())
        );
        assert_eq!(
            resolve_base_branch(None, Some("project"), Some("global")),
            Some("project".to_string())
        );
        assert_eq!(
            resolve_base_branch(None, None, Some("global")),
            Some("global".to_string())
        );
        assert_eq!(resolve_base_branch(None, None, None), None);
        assert_eq!(
            resolve_base_branch(Some("   "), Some(""), Some("global")),
            Some("global".to_string())
        );
        assert_eq!(resolve_base_branch(Some("  "), None, None), None);
    }

    #[test]
    fn resolve_repo_base_branch_keys_launch_repo_by_root() {
        let (parent, _tip) = init_repo_with_branch("proj", "release");
        let root = parent.path().join("proj");
        let key = crate::session::projects::canonical_key(&root.to_string_lossy());
        let mut bases = std::collections::HashMap::new();
        bases.insert(key, "develop".to_string());

        assert_eq!(
            resolve_repo_base_branch(&root, None, &bases, Some("global")),
            Some("develop".to_string())
        );

        assert_eq!(
            resolve_repo_base_branch(&root, Some("hotfix"), &bases, Some("global")),
            Some("hotfix".to_string())
        );

        let empty = std::collections::HashMap::new();
        assert_eq!(
            resolve_repo_base_branch(&root, None, &empty, Some("global")),
            Some("global".to_string())
        );

        // Launching from a linked worktree still keys by the main repo root.
        let wt_path = parent.path().join("proj-wt");
        GitWorktree::new(root.clone())
            .unwrap()
            .create_worktree("wt-branch", &wt_path, true, None)
            .unwrap();
        assert_eq!(
            resolve_repo_base_branch(&wt_path, None, &bases, None),
            Some("develop".to_string())
        );
    }

    fn init_repo_with_branch(name: &str, branch: &str) -> (tempfile::TempDir, git2::Oid) {
        let parent = tempfile::Builder::new()
            .prefix("aoe-test-")
            .tempdir()
            .unwrap();
        let dir = parent.path().join(name);
        std::fs::create_dir(&dir).unwrap();
        let repo = git2::Repository::init(&dir).unwrap();
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();

        std::fs::write(dir.join("README.md"), format!("{name}\n")).unwrap();
        let tree_id = {
            let mut index = repo.index().unwrap();
            index.add_path(std::path::Path::new("README.md")).unwrap();
            index.write_tree().unwrap()
        };
        let tree = repo.find_tree(tree_id).unwrap();
        let base_commit = repo
            .commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();

        let base = repo.find_commit(base_commit).unwrap();
        repo.branch(branch, &base, false).unwrap();
        std::fs::write(dir.join("RELEASE.md"), "release\n").unwrap();
        let tree_id = {
            let mut index = repo.index().unwrap();
            index.add_path(std::path::Path::new("RELEASE.md")).unwrap();
            index.write_tree().unwrap()
        };
        let tree = repo.find_tree(tree_id).unwrap();
        let branch_ref = format!("refs/heads/{branch}");
        let release_commit = repo
            .commit(Some(&branch_ref), &sig, &sig, "release", &tree, &[&base])
            .unwrap();

        (parent, release_commit)
    }

    #[test]
    #[serial_test::serial]
    fn create_workspace_honors_per_repo_base_branch() {
        let home = tempfile::tempdir().unwrap();
        let _home_guard = crate::session::test_support::isolate_home(home.path());
        let storage = super::super::Storage::new_unwatched("default").unwrap();
        let (parent_primary, _) = init_repo_with_branch("primary", "release");
        let (parent_extra, extra_release_tip) = init_repo_with_branch("extra", "release");
        let primary = parent_primary.path().join("primary");
        let extra = parent_extra.path().join("extra");

        let workspaces_root = tempfile::TempDir::new().unwrap();
        let template = workspaces_root
            .path()
            .join("{branch}")
            .to_string_lossy()
            .into_owned();
        let mut prepared = Instance::new("workspace bases", primary.to_str().unwrap());
        prepared.storage_origin = Some(std::sync::Arc::new(storage.clone()));

        let result = create_workspace(
            &WorkspaceRepoSpec {
                path: primary,
                base_branch: None,
            },
            &[WorkspaceRepoSpec {
                path: extra,
                base_branch: Some("release".to_string()),
            }],
            "feature-x",
            true,
            &template,
            true,
            &mut prepared,
        )
        .expect("workspace creation should succeed");

        let extra_repo = result
            .workspace_info
            .repos
            .iter()
            .find(|r| r.name == "extra")
            .expect("extra repo present in workspace");
        let wt = git2::Repository::open(&extra_repo.worktree_path).unwrap();
        let head = wt.head().unwrap().peel_to_commit().unwrap();
        assert_eq!(
            head.id(),
            extra_release_tip,
            "extra repo worktree should branch from its configured `release` base"
        );
        assert_eq!(extra_repo.base_branch.as_deref(), Some("release"));
        assert_eq!(
            result
                .workspace_info
                .repos
                .iter()
                .find(|r| r.name == "primary")
                .unwrap()
                .base_branch,
            None,
            "a repo with no configured base records none, so the diff falls through to detection"
        );
    }

    #[test]
    #[serial_test::serial]
    fn create_workspace_records_no_base_when_attaching_an_existing_branch() {
        let home = tempfile::tempdir().unwrap();
        let _home_guard = crate::session::test_support::isolate_home(home.path());
        let storage = super::super::Storage::new_unwatched("default").unwrap();
        let (parent_primary, _) = init_repo_with_branch("primary", "feature-x");
        let primary = parent_primary.path().join("primary");
        let workspaces_root = tempfile::TempDir::new().unwrap();
        let template = workspaces_root
            .path()
            .join("{branch}")
            .to_string_lossy()
            .into_owned();
        let mut prepared = Instance::new("existing branch workspace", primary.to_str().unwrap());
        prepared.storage_origin = Some(std::sync::Arc::new(storage.clone()));

        let result = create_workspace(
            &WorkspaceRepoSpec {
                path: primary,
                base_branch: Some("main".to_string()),
            },
            &[],
            "feature-x",
            false,
            &template,
            true,
            &mut prepared,
        )
        .expect("workspace creation should succeed");

        assert_eq!(result.workspace_info.repos[0].base_branch, None);
    }

    #[test]
    fn resolve_repo_base_selectors_matches_name_or_path() {
        let repos = vec![
            PathBuf::from("/src/app"),
            PathBuf::from("/src/api"),
            PathBuf::from("/elsewhere/web"),
        ];

        let out = resolve_repo_base_selectors(
            &repos,
            &[
                ("api".to_string(), "epic/checkout".to_string()),
                ("/elsewhere/web".to_string(), " develop ".to_string()),
            ],
        )
        .expect("both selectors resolve");
        assert_eq!(
            out.get(&PathBuf::from("/src/api")).map(String::as_str),
            Some("epic/checkout")
        );
        assert_eq!(
            out.get(&PathBuf::from("/elsewhere/web"))
                .map(String::as_str),
            Some("develop")
        );
        assert!(!out.contains_key(&PathBuf::from("/src/app")));

        assert!(resolve_repo_base_selectors(&repos, &[]).unwrap().is_empty());

        let cases = [
            (
                vec![("nope".to_string(), "develop".to_string())],
                "No repo named",
            ),
            (
                vec![("api".to_string(), "  ".to_string())],
                "No base branch",
            ),
            (
                vec![
                    ("api".to_string(), "develop".to_string()),
                    ("/src/api".to_string(), "main".to_string()),
                ],
                "twice",
            ),
        ];
        for (pairs, expected) in cases {
            let err = resolve_repo_base_selectors(&repos, &pairs)
                .expect_err("should reject")
                .to_string();
            assert!(err.contains(expected), "got: {err}");
        }

        let subdir = vec![PathBuf::from("/src/api/crates/core")];
        assert!(
            resolve_repo_base_selectors(&subdir, &[("api".to_string(), "develop".to_string())])
                .is_err(),
            "a repo name must not resolve against a subdirectory path"
        );
        assert!(resolve_repo_base_selectors(
            &subdir,
            &[("core".to_string(), "develop".to_string())]
        )
        .is_ok());

        let dupes = vec![PathBuf::from("/a/api"), PathBuf::from("/b/api")];
        let err = resolve_repo_base_selectors(&dupes, &[("api".to_string(), "x".to_string())])
            .expect_err("ambiguous name")
            .to_string();
        assert!(err.contains("ambiguous"), "got: {err}");
    }

    fn isolated_app_dir(temp_home: &std::path::Path) -> std::path::PathBuf {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let config_home = temp_home.join(".config");

            config_home.join(crate::session::APP_DIR_NAME_XDG)
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            temp_home.join(crate::session::APP_DIR_NAME_OTHER)
        }
    }

    fn custom_agent_params(project_path: &std::path::Path, tool: &str) -> InstanceParams {
        InstanceParams {
            title: format!("{tool} session"),
            title_typed: false,
            path: project_path.to_string_lossy().to_string(),
            group: String::new(),
            tool: tool.to_string(),
            worktree_enabled: false,
            worktree_branch: None,
            create_new_branch: false,
            base_branch: None,
            sandbox: false,
            sandbox_image: "ubuntu:latest".to_string(),
            yolo_mode: false,
            extra_env: Vec::new(),
            extra_args: String::new(),
            command_override: String::new(),
            extra_repo_paths: Vec::new(),
            repo_base_branches: Vec::new(),
            scratch: false,
            fork_seed: None,
        }
    }

    #[test]
    fn apply_agent_launch_config_prefers_set_session_values_over_config() {
        // (session extra, config extra, session command, config override,
        //  session yolo, config yolo) -> (extra, command, yolo)
        let cases = [
            (("", None, "", None, None, false), ("", "claude", false)),
            (
                ("", Some("--cfg"), "", Some("wrap"), None, true),
                ("--cfg", "wrap", true),
            ),
            (
                ("", Some(""), "", Some(""), None, false),
                ("", "claude", false),
            ),
            (
                (
                    "--mine",
                    Some("--cfg"),
                    "mine",
                    Some("wrap"),
                    Some(false),
                    true,
                ),
                ("--mine", "mine", false),
            ),
        ];
        for ((extra, cfg_extra, cmd, cfg_cmd, yolo, cfg_yolo), expected) in cases {
            let mut session = crate::session::config::SessionConfig {
                yolo_mode_default: cfg_yolo,
                ..Default::default()
            };
            if let Some(v) = cfg_extra {
                session.agent_extra_args.insert("claude".into(), v.into());
            }
            if let Some(v) = cfg_cmd {
                session
                    .agent_command_override
                    .insert("claude".into(), v.into());
            }
            let mut inst = Instance::new("t", "/p");
            inst.tool = "claude".into();
            inst.command = "claude".into();
            apply_agent_launch_config(&mut inst, &session, extra, cmd, yolo);
            assert_eq!(
                (
                    inst.extra_args.as_str(),
                    inst.command.as_str(),
                    inst.yolo_mode
                ),
                expected,
                "extra={extra:?} cfg_extra={cfg_extra:?} cmd={cmd:?} cfg_cmd={cfg_cmd:?}"
            );
        }
    }

    #[test]
    #[serial_test::serial]
    fn build_instance_resolves_custom_agent_commands_and_detect_as() {
        let temp_home = tempfile::tempdir().unwrap();
        let _home_guard = crate::session::test_support::isolate_home(temp_home.path());
        let app_dir = isolated_app_dir(temp_home.path());
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::write(
            app_dir.join("config.toml"),
            r#"
                [session.custom_agents]
                remote-claude = "ssh -t host claude"
                remote-opencode = "ssh -t host opencode"

                whitespace-agent = "   "

                [session.agent_detect_as]
                remote-claude = "claude"
            "#,
        )
        .unwrap();
        let project = tempfile::tempdir().unwrap();
        let _registry = crate::tmux::status_rules::ProfileRegistryGuard::take("default");
        let storage = crate::session::Storage::new_unwatched("default").unwrap();

        let result = build_instance(
            custom_agent_params(project.path(), "remote-claude"),
            &[],
            &[],
            &storage,
        )
        .unwrap();

        assert_eq!(result.instance.tool, "remote-claude");
        assert_eq!(result.instance.command, "ssh -t host claude");
        assert_eq!(result.instance.detect_as, "claude");

        let unmapped = build_instance(
            custom_agent_params(project.path(), "remote-opencode"),
            &[],
            &[],
            &storage,
        )
        .unwrap();
        assert_eq!(unmapped.instance.command, "ssh -t host opencode");
        assert_eq!(unmapped.instance.detect_as, "");

        for tool in ["remote-missing", "whitespace-agent"] {
            let Err(err) = build_instance(
                custom_agent_params(project.path(), tool),
                &[],
                &[],
                &storage,
            ) else {
                panic!("{tool}: custom agent without a command should fail");
            };
            assert!(
                err.to_string().contains(&format!(
                    "No launch command resolved for custom agent '{tool}'"
                )),
                "unexpected error: {err}"
            );
        }
    }

    #[test]
    #[serial_test::serial]
    fn build_instance_provisions_scratch_and_rejects_invalid_worktree_requests() {
        let temp_home = tempfile::tempdir().unwrap();
        let _home_guard = crate::session::test_support::isolate_home(temp_home.path());
        let app_dir = isolated_app_dir(temp_home.path());
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::write(app_dir.join("config.toml"), "").unwrap();
        let storage = crate::session::Storage::new_unwatched("default").unwrap();

        let mut params = custom_agent_params(std::path::Path::new(""), "claude");
        params.scratch = true;
        let result = build_instance(params.clone(), &[], &[], &storage)
            .expect("scratch build must succeed without a project path");
        assert!(
            result.instance.scratch,
            "scratch flag must be persisted on the instance"
        );
        let provisioned = std::path::PathBuf::from(&result.instance.project_path);
        assert!(provisioned.exists());
        assert!(super::super::scratch::is_scratch_path(&provisioned));
        let _ = std::fs::remove_dir_all(&provisioned);

        params.worktree_enabled = true;
        params.worktree_branch = Some("feat".to_string());
        let Err(err) = build_instance(params, &[], &[], &storage) else {
            panic!("scratch + worktree must error");
        };
        assert!(
            err.to_string()
                .contains("Cannot combine --scratch with worktree mode"),
            "unexpected error: {err}"
        );

        let project = tempfile::tempdir().unwrap();
        let mut params = custom_agent_params(project.path(), "claude");
        params.worktree_enabled = true;
        params.worktree_branch = Some("feat".to_string());
        let Err(err) = build_instance(params, &[], &[], &storage) else {
            panic!("worktree on a non-git path must error");
        };
        assert!(
            err.chain()
                .filter_map(|c| c.downcast_ref::<crate::git::error::GitError>())
                .any(|g| matches!(g, crate::git::error::GitError::NotAGitRepo)),
            "expected a typed GitError::NotAGitRepo in the chain, got: {err:#}"
        );
    }

    fn build_instance_applies_structured_fork_seed() {
        use crate::session::ForkSeed;
        let _registry = crate::tmux::status_rules::ProfileRegistryGuard::take("default");
        let storage = crate::session::Storage::new_unwatched("default").unwrap();
        let params = InstanceParams {
            title: "Structured fork child".into(),
            title_typed: false,
            path: "/tmp".into(),
            group: String::new(),
            tool: "claude".into(),
            worktree_enabled: false,
            worktree_branch: None,
            create_new_branch: false,
            base_branch: None,
            sandbox: false,
            sandbox_image: String::new(),
            yolo_mode: false,
            extra_env: vec![],
            extra_args: String::new(),
            command_override: String::new(),
            extra_repo_paths: vec![],
            repo_base_branches: Vec::new(),
            scratch: false,
            fork_seed: Some(ForkSeed::Structured {
                parent_acp_session_id: "parent-acp-id".into(),
            }),
        };
        let inst = build_instance(params, &[], &[], &storage).unwrap().instance;
        assert_eq!(inst.view, crate::session::View::Structured);
        assert_eq!(inst.fork_pending.as_deref(), Some("parent-acp-id"));
        assert_eq!(inst.import_pending, Some(true));
        assert!(inst.agent_session_id.is_none());
        assert!(!matches!(
            inst.resume_intent,
            crate::session::instance::ResumeIntent::Fork { .. }
        ));
    }

    fn build_instance_applies_terminal_fork_seed() {
        use crate::session::ForkSeed;
        let _registry = crate::tmux::status_rules::ProfileRegistryGuard::take("default");
        let storage = crate::session::Storage::new_unwatched("default").unwrap();
        // The CLI e2e covers the separate application in `add.rs`; this is the
        // arm `build_instance` owns, which pins the child conversation and the
        // parent the first launch must fork from.
        let parent = crate::session::ConversationBinding {
            session_id: "parent-conversation".into(),
            execution: Some(crate::session::ExecutionBinding {
                agent: "claude".into(),
                stores: vec![std::path::PathBuf::from("/tmp/store")],
                configuration: Vec::new(),
                cwd: "/tmp".into(),
                cwd_filesystem: "host".into(),
                filesystem: "host".into(),
                exported_default_store: None,
            }),
            provenance: crate::session::ConversationProvenance::Observed,
            transcript_path: None,
        };
        let params = InstanceParams {
            title: "Terminal fork child".into(),
            title_typed: false,
            path: "/tmp".into(),
            group: String::new(),
            tool: "claude".into(),
            worktree_enabled: false,
            worktree_branch: None,
            create_new_branch: false,
            base_branch: None,
            sandbox: false,
            sandbox_image: String::new(),
            yolo_mode: false,
            extra_env: vec![],
            extra_args: String::new(),
            command_override: String::new(),
            extra_repo_paths: vec![],
            repo_base_branches: Vec::new(),
            scratch: false,
            fork_seed: Some(ForkSeed::Terminal {
                parent: Box::new(parent.clone()),
                child_session_id: "child-conversation".into(),
                unattributed_parent_agent: None,
            }),
        };
        let inst = build_instance(params, &[], &[], &storage).unwrap().instance;
        assert_eq!(inst.agent_session_id.as_deref(), Some("child-conversation"));
        assert_eq!(
            inst.resume_intent,
            crate::session::ResumeIntent::Fork {
                from: "parent-conversation".into()
            }
        );
        assert_eq!(inst.resume_binding.as_ref(), Some(&parent));
    }

    #[test]
    #[serial_test::serial]
    fn fork_seed_builds_apply_the_seed_and_restore_default_profile_registry() {
        let _app_guard = crate::session::test_support::isolate_app_dir();
        const ALIAS_AGENT: &str = "fork-seed-registry-alias";
        const RULE_AGENT: &str = "fork-seed-registry-rule";
        let cases: &[(&str, fn())] = &[
            ("terminal", build_instance_applies_terminal_fork_seed),
            ("structured", build_instance_applies_structured_fork_seed),
        ];

        for (label, run) in cases {
            let _cleanup = crate::tmux::status_rules::ProfileRegistryGuard::take("default");
            let mut sentinels = crate::session::Config::default();
            sentinels
                .session
                .agent_detect_as
                .insert(ALIAS_AGENT.to_string(), "codex".to_string());
            sentinels
                .agents
                .entry(RULE_AGENT.to_string())
                .or_default()
                .status_rules = vec![crate::session::config::StatusRule {
                status: crate::agents::HookStatus::Running,
                contains: Some("fork-seed-working".to_string()),
                regex: None,
            }];
            crate::tmux::status_rules::install_from_config("default", &sentinels);

            run();

            let alias = crate::tmux::status_rules::effective_detect_as("default", ALIAS_AGENT, "");
            let rule =
                crate::tmux::status_rules::detect("default", RULE_AGENT, "fork-seed-working");
            assert_eq!(
                (alias.as_ref(), rule),
                ("codex", Some(crate::session::Status::Running)),
                "{label}: fork-seed build must restore the prior alias and compiled rule"
            );
        }
    }

    /// The parent's capability is checked against the parent row's own agent,
    /// and the launch skips identity checking for an unattributed binding, so
    /// a child that would launch another agent must be refused here rather
    /// than fork a conversation it cannot resume.
    #[test]
    #[serial_test::serial]
    fn a_fork_child_that_would_launch_another_agent_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(root.path());
        let storage = crate::session::Storage::new_unwatched("default").unwrap();
        let mut parent = crate::session::Instance::new("parent", root.path().to_str().unwrap());
        parent.tool = "claude".into();
        parent.agent_session_id = Some("legacy-uuid".into());
        parent.agent_session_binding =
            Some(crate::session::ConversationBinding::unknown("legacy-uuid"));
        let seed = crate::session::fork::terminal_fork_seed(
            parent.fork_parent_ref().unwrap(),
            "child-uuid".into(),
        )
        .expect("an unattributed parent is admitted");

        let mut same_agent = custom_agent_params(root.path(), "claude");
        same_agent.command_override = "claude".into();
        same_agent.fork_seed = Some(seed.clone());
        assert_eq!(
            build_instance(same_agent, &[], &[], &storage)
                .expect("a child launching the parent's own agent still forks")
                .instance
                .agent_session_id
                .as_deref(),
            Some("child-uuid")
        );

        let mut other_agent = custom_agent_params(root.path(), "codex");
        other_agent.command_override = "codex".into();
        other_agent.fork_seed = Some(seed);
        let refused = match build_instance(other_agent, &[], &[], &storage) {
            Ok(_) => panic!("a child launching another agent cannot carry the conversation"),
            Err(error) => error.to_string(),
        };
        assert!(
            refused.contains("codex") && refused.contains("claude"),
            "the refusal must name both agents: {refused}"
        );
    }
}
