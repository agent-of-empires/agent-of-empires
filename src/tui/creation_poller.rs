//! Background session creation handler for TUI responsiveness
//!
//! This handles the potentially slow Docker operations (image pull, container creation)
//! in a background thread so the UI remains responsive.

use std::sync::mpsc;
use std::thread;

use tokio_util::sync::CancellationToken;

use crate::session::builder::{self, CreatedWorktree, InstanceParams};
use crate::session::config::repo_config::{self, HookProgress, ResolvedHooks};
use crate::session::Instance;
use crate::tui::dialogs::NewSessionData;

pub struct CreationRequest {
    pub data: NewSessionData,
    pub existing_instances: Vec<Instance>,
    /// Trusted hooks to execute after instance creation (already approved by user).
    pub hooks: Option<ResolvedHooks>,
    pub cancel: CancellationToken,
}

pub struct CreationSuccess {
    pub profile_storage: crate::session::Storage,
    pub witness: builder::CreationWitness,
    pub session_id: String,
    pub instance: Instance,
    pub created_worktree: Option<CreatedWorktreeInfo>,
    pub created_workspace_worktrees: Vec<CreatedWorktreeInfo>,
    pub on_launch_hooks_ran: bool,
    pub warnings: Vec<String>,
}

pub enum CreationResult {
    Success(Box<CreationSuccess>),
    Error(String),
    /// The worker already rolled back the cancelled build.
    Cancelled,
}

impl std::fmt::Debug for CreationResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Success(success) => f
                .debug_struct("Success")
                .field("session_id", &success.session_id)
                .field("profile", &success.profile_storage.profile())
                .finish_non_exhaustive(),
            Self::Error(error) => f.debug_tuple("Error").field(error).finish(),
            Self::Cancelled => f.write_str("Cancelled"),
        }
    }
}

pub struct CreationOutcome {
    pub result: CreationResult,
    /// Cancelled after the worker's last check: a `Success` still needs rollback.
    pub cancelled: bool,
}

/// Worktree cleanup information and pinned originals passed across the worker boundary.
#[derive(Debug, Clone)]
pub struct CreatedWorktreeInfo {
    pub path: String,
    pub main_repo_path: String,
    pub owned_branch: Option<String>,
    pub(crate) witness: builder::CheckoutWitness,
}

impl From<&CreatedWorktree> for CreatedWorktreeInfo {
    fn from(wt: &CreatedWorktree) -> Self {
        Self {
            path: wt.path.to_string_lossy().to_string(),
            main_repo_path: wt.main_repo_path.to_string_lossy().to_string(),
            owned_branch: wt.owned_branch.clone(),
            witness: wt.witness.clone(),
        }
    }
}

impl From<&CreatedWorktreeInfo> for CreatedWorktree {
    fn from(worktree: &CreatedWorktreeInfo) -> Self {
        Self {
            path: worktree.path.as_str().into(),
            main_repo_path: worktree.main_repo_path.as_str().into(),
            owned_branch: worktree.owned_branch.clone(),
            witness: worktree.witness.clone(),
        }
    }
}

pub struct CreationPoller {
    request_tx: mpsc::Sender<(CreationRequest, mpsc::Sender<HookProgress>)>,
    result_rx: mpsc::Receiver<(CreationResult, CancellationToken)>,
    progress_rx: mpsc::Receiver<HookProgress>,
    progress_tx: mpsc::Sender<HookProgress>,
    _handle: thread::JoinHandle<()>,
    /// Requests sent and not yet received, including cancelled ones still winding down.
    in_flight: usize,
    #[cfg(test)]
    result_queued_observer: std::sync::Arc<std::sync::Mutex<Option<mpsc::Sender<()>>>>,
}

/// Appends which config file declared the failing `on_create` commands.
fn on_create_error(e: &anyhow::Error, hooks: &ResolvedHooks) -> String {
    let msg = format!("on_create hook failed: {e:#}");
    match hooks.origin_hint("on_create") {
        Some(hint) => format!("{msg}\n{hint}"),
        None => msg,
    }
}

impl CreationPoller {
    pub fn new() -> Self {
        let (request_tx, request_rx) =
            mpsc::channel::<(CreationRequest, mpsc::Sender<HookProgress>)>();
        let (result_tx, result_rx) = mpsc::channel::<(CreationResult, CancellationToken)>();
        let (progress_tx, progress_rx) = mpsc::channel::<HookProgress>();
        #[cfg(test)]
        let result_queued_observer =
            std::sync::Arc::new(std::sync::Mutex::new(None::<mpsc::Sender<()>>));
        #[cfg(test)]
        let queued_observer = result_queued_observer.clone();

        let handle = thread::spawn(move || {
            while let Ok((request, prog_tx)) = request_rx.recv() {
                let cancel = request.cancel.clone();
                let result = Self::create_instance(request, &prog_tx);
                if result_tx.send((result, cancel)).is_err() {
                    break;
                }
                #[cfg(test)]
                if let Some(observer) = queued_observer.lock().unwrap().take() {
                    let _ = observer.send(());
                }
            }
        });

        Self {
            request_tx,
            result_rx,
            progress_rx,
            progress_tx,
            _handle: handle,
            in_flight: 0,
            #[cfg(test)]
            result_queued_observer,
        }
    }

    fn create_instance(
        request: CreationRequest,
        progress_tx: &mpsc::Sender<HookProgress>,
    ) -> CreationResult {
        let data = request.data;
        let hooks = request.hooks;
        let cancel = request.cancel;
        if cancel.is_cancelled() {
            return CreationResult::Cancelled;
        }
        let profile = data.profile.clone();
        let sandbox = data.sandbox;

        let existing_titles: Vec<&str> = request
            .existing_instances
            .iter()
            .map(|i| i.title.as_str())
            .collect();
        let existing_branches: Vec<&str> = request
            .existing_instances
            .iter()
            .filter_map(|i| i.worktree_info.as_ref().map(|w| w.branch.as_str()))
            .collect();

        // `structured` is applied post-build (mirrors the web create
        // handler); read it off before the params conversion consumes data.
        let structured = data.structured;
        let params = InstanceParams::from(data);

        let build_result =
            match builder::build_instance(params, &existing_titles, &existing_branches, &profile) {
                Ok(r) => r,
                Err(e) => return CreationResult::Error(format!("{:#}", e)),
            };

        let ownership = std::cell::RefCell::new(build_result.ownership);
        let profile_storage = build_result.profile_storage;
        let witness = build_result.witness;
        let mut instance = build_result.instance;
        // Tag the instance with its profile NOW, before container creation or any
        // hook execution. Downstream config-resolution sites (build_container_config,
        // on_launch hook resolution, build_docker_env_args) read source_profile to
        // pick the right profile's overrides; if it's left blank they'd silently
        // fall back to the global default profile.
        instance.source_profile = profile.clone();
        if structured {
            builder::structured::apply_structured_choice(&mut instance);
        }
        let created_worktree = build_result.created_worktree;
        let created_workspace_worktrees = build_result.created_workspace_worktrees;
        let warnings = build_result.warnings;
        let roll_back = |instance: &Instance| {
            drop(ownership.borrow_mut().take());
            builder::cleanup_unpublished_instance(
                instance,
                created_worktree.as_ref(),
                &created_workspace_worktrees,
                None,
                &witness,
            )
        };
        let cancelled = |instance: &Instance| {
            roll_back(instance);
            CreationResult::Cancelled
        };
        // A step that fails because the user cancelled reports the cancel, not the error.
        let failed = |instance: &Instance, message: String| {
            if cancel.is_cancelled() {
                return cancelled(instance);
            }
            roll_back(instance);
            CreationResult::Error(message)
        };
        if cancel.is_cancelled() {
            return cancelled(&instance);
        }

        let has_on_create = hooks
            .as_ref()
            .is_some_and(|h| !h.hooks().on_create.is_empty());
        let has_on_launch = hooks
            .as_ref()
            .is_some_and(|h| !h.hooks().on_launch.is_empty());
        let mut container_started = false;
        let hook_env = repo_config::lifecycle_env_vars(&instance);

        if has_on_create || has_on_launch {
            drop(ownership.borrow_mut().take());
        }
        // Execute on_create hooks after worktree setup, before starting
        if has_on_create {
            let hooks = hooks.as_ref().unwrap();
            if sandbox {
                // Ensure the container is running so we can exec hooks inside it.
                // Don't create the tmux session yet -- that happens at attach time
                // where the terminal size is available.
                if let Err(e) = instance.get_container_until_cancelled(&cancel) {
                    return failed(&instance, format!("{:#}", e));
                }
                container_started = true;
                if cancel.is_cancelled() {
                    return cancelled(&instance);
                }
                if let Some(ref sandbox) = instance.sandbox_info {
                    let workdir = instance.container_workdir();
                    if let Err(e) = repo_config::execute_hooks_in_container_streamed(
                        &hooks.hooks().on_create,
                        &sandbox.container_name,
                        &workdir,
                        progress_tx,
                        &hook_env,
                    ) {
                        tracing::warn!(target: "session.create", "on_create hook failed in container: {:#}", e);
                        return failed(&instance, on_create_error(&e, hooks));
                    }
                }
            } else if let Err(e) = repo_config::execute_hooks_streamed(
                &hooks.hooks().on_create,
                std::path::Path::new(&instance.project_path),
                progress_tx,
                &hook_env,
            ) {
                return failed(&instance, on_create_error(&e, hooks));
            }
        }

        // Execute on_launch hooks in background too (non-fatal, like start_with_size).
        // This prevents blocking the UI thread when the session is first attached.
        if has_on_launch {
            // A cancel during on_create must not start the next phase.
            if cancel.is_cancelled() {
                return cancelled(&instance);
            }
            let hooks = hooks.as_ref().unwrap();
            if sandbox {
                if !container_started {
                    if let Err(e) = instance.get_container_until_cancelled(&cancel) {
                        if cancel.is_cancelled() {
                            return cancelled(&instance);
                        }
                        let msg = format!("Container startup warning: {:#}", e);
                        tracing::warn!(target: "session.create", "{}", msg);
                        let _ = progress_tx.send(HookProgress::Output(msg));
                    } else {
                        container_started = true;
                        if cancel.is_cancelled() {
                            return cancelled(&instance);
                        }
                    }
                }
                if container_started {
                    if let Some(ref sandbox) = instance.sandbox_info {
                        let workdir = instance.container_workdir();
                        if let Err(e) = repo_config::execute_hooks_in_container_streamed(
                            &hooks.hooks().on_launch,
                            &sandbox.container_name,
                            &workdir,
                            progress_tx,
                            &hook_env,
                        ) {
                            tracing::warn!(target: "session.create", "on_launch hook failed in container: {}", e);
                        }
                    }
                }
            } else if let Err(e) = repo_config::execute_hooks_streamed(
                &hooks.hooks().on_launch,
                std::path::Path::new(&instance.project_path),
                progress_tx,
                &hook_env,
            ) {
                tracing::warn!(target: "session.create", "on_launch hook failed: {}", e);
            }
        }

        if sandbox && !container_started {
            drop(ownership.borrow_mut().take());
            if let Err(error) = instance.get_container_until_cancelled(&cancel) {
                return failed(&instance, format!("{error:#}"));
            }
        }
        if cancel.is_cancelled() {
            return cancelled(&instance);
        }
        if ownership.borrow().is_none() {
            match crate::session::storage::acquire_ownership_read() {
                Ok(guard) => *ownership.borrow_mut() = Some(guard),
                Err(error) => return failed(&instance, format!("{error:#}")),
            }
        }
        if let Err(error) = profile_storage.verify_profile_identity() {
            return failed(&instance, format!("{error:#}"));
        }
        if let Err(error) = witness.validate(&instance) {
            return failed(&instance, format!("{error:#}"));
        }

        let created_worktree_info = created_worktree.as_ref().map(CreatedWorktreeInfo::from);
        let created_workspace_worktree_info = created_workspace_worktrees
            .iter()
            .map(CreatedWorktreeInfo::from)
            .collect();

        // A queued root flock would deadlock an exclusive UI action before polling.
        drop(ownership.into_inner());
        CreationResult::Success(Box::new(CreationSuccess {
            profile_storage,
            witness,
            session_id: instance.id.clone(),
            instance,
            created_worktree: created_worktree_info,
            created_workspace_worktrees: created_workspace_worktree_info,
            on_launch_hooks_ran: has_on_launch,
            warnings,
        }))
    }

    pub fn request_creation(&mut self, request: CreationRequest) {
        // The result carries the original Storage rather than a mutable last-profile hint.
        if self
            .request_tx
            .send((request, self.progress_tx.clone()))
            .is_err()
        {
            tracing::error!(target: "session.create", "Failed to send creation request: receiver thread died");
        } else {
            self.in_flight += 1;
        }
    }

    #[cfg(test)]
    pub(crate) fn observe_next_queued_result(&self) -> mpsc::Receiver<()> {
        let (sender, receiver) = mpsc::channel();
        *self.result_queued_observer.lock().unwrap() = Some(sender);
        receiver
    }

    pub fn try_recv_result(&mut self) -> Option<CreationOutcome> {
        self.received(self.result_rx.try_recv().ok())
    }

    /// Blocking receive with timeout, used during shutdown cleanup.
    pub fn recv_result_timeout(&mut self, timeout: std::time::Duration) -> Option<CreationOutcome> {
        self.received(self.result_rx.recv_timeout(timeout).ok())
    }

    fn received(
        &mut self,
        received: Option<(CreationResult, CancellationToken)>,
    ) -> Option<CreationOutcome> {
        let (result, cancel) = received?;
        self.in_flight = self.in_flight.saturating_sub(1);
        Some(CreationOutcome {
            result,
            cancelled: cancel.is_cancelled(),
        })
    }

    pub fn try_recv_progress(&self) -> Option<HookProgress> {
        self.progress_rx.try_recv().ok()
    }

    pub fn is_pending(&self) -> bool {
        self.in_flight > 0
    }
}

impl Default for CreationPoller {
    fn default() -> Self {
        Self::new()
    }
}
