//! Background session creation handler for TUI responsiveness
//!
//! This handles the potentially slow Docker operations (image pull, container creation)
//! in a background thread so the UI remains responsive.

use std::sync::mpsc;
use std::thread;

use crate::session::builder::{self, CreatedWorktree, InstanceParams};
use crate::session::config::repo_config::{self, HookProgress, ResolvedHooks};
use crate::session::Instance;
use crate::tui::dialogs::NewSessionData;

pub struct CreationRequest {
    pub data: NewSessionData,
    pub existing_instances: Vec<Instance>,
    /// Trusted hooks to execute after instance creation (already approved by user).
    pub hooks: Option<ResolvedHooks>,
}

#[derive(Debug)]
pub enum CreationResult {
    Success {
        session_id: String,
        instance: Box<Instance>,
        created_worktree: Option<CreatedWorktreeInfo>,
        /// Workspace worktrees created during build, needed for rollback.
        created_workspace_worktrees: Vec<CreatedWorktreeInfo>,
        on_launch_hooks_ran: bool,
        /// Non-fatal warnings from worktree creation (e.g. post-checkout hook
        /// failures). Surfaced as a transient toast in the UI.
        warnings: Vec<String>,
    },
    Error(String),
}

/// Serializable worktree info for passing across thread boundary
#[derive(Debug, Clone)]
pub struct CreatedWorktreeInfo {
    pub path: String,
    pub main_repo_path: String,
    pub owned_branch: Option<String>,
}

impl From<&CreatedWorktree> for CreatedWorktreeInfo {
    fn from(wt: &CreatedWorktree) -> Self {
        Self {
            path: wt.path.to_string_lossy().to_string(),
            main_repo_path: wt.main_repo_path.to_string_lossy().to_string(),
            owned_branch: wt.owned_branch.clone(),
        }
    }
}

impl From<&CreatedWorktreeInfo> for CreatedWorktree {
    fn from(worktree: &CreatedWorktreeInfo) -> Self {
        Self {
            path: worktree.path.as_str().into(),
            main_repo_path: worktree.main_repo_path.as_str().into(),
            owned_branch: worktree.owned_branch.clone(),
        }
    }
}

pub struct CreationPoller {
    request_tx: mpsc::Sender<(CreationRequest, mpsc::Sender<HookProgress>)>,
    result_rx: mpsc::Receiver<CreationResult>,
    progress_rx: mpsc::Receiver<HookProgress>,
    progress_tx: mpsc::Sender<HookProgress>,
    _handle: thread::JoinHandle<()>,
    pending: bool,
    /// Profile from the last creation request (for cross-profile saves)
    last_profile: Option<String>,
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
        let (result_tx, result_rx) = mpsc::channel::<CreationResult>();
        let (progress_tx, progress_rx) = mpsc::channel::<HookProgress>();

        let handle = thread::spawn(move || {
            while let Ok((request, prog_tx)) = request_rx.recv() {
                let result = Self::create_instance(request, &prog_tx);
                if result_tx.send(result).is_err() {
                    break;
                }
            }
        });

        Self {
            request_tx,
            result_rx,
            progress_rx,
            progress_tx,
            _handle: handle,
            pending: false,
            last_profile: None,
        }
    }

    fn create_instance(
        request: CreationRequest,
        progress_tx: &mpsc::Sender<HookProgress>,
    ) -> CreationResult {
        let data = request.data;
        let hooks = request.hooks;
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

        let has_on_create = hooks
            .as_ref()
            .is_some_and(|h| !h.hooks().on_create.is_empty());
        let has_on_launch = hooks
            .as_ref()
            .is_some_and(|h| !h.hooks().on_launch.is_empty());
        let mut container_started = false;
        let hook_env = repo_config::lifecycle_env_vars(&instance);

        // Execute on_create hooks after worktree setup, before starting
        if has_on_create {
            let hooks = hooks.as_ref().unwrap();
            if sandbox {
                // Ensure the container is running so we can exec hooks inside it.
                // Don't create the tmux session yet -- that happens at attach time
                // where the terminal size is available.
                if let Err(e) = instance.get_container_for_instance() {
                    builder::cleanup_instance(
                        &instance,
                        created_worktree.as_ref(),
                        &created_workspace_worktrees,
                        None,
                    );
                    return CreationResult::Error(format!("{:#}", e));
                }
                container_started = true;
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
                        builder::cleanup_instance(
                            &instance,
                            created_worktree.as_ref(),
                            &created_workspace_worktrees,
                            None,
                        );
                        return CreationResult::Error(on_create_error(&e, hooks));
                    }
                }
            } else if let Err(e) = repo_config::execute_hooks_streamed(
                &hooks.hooks().on_create,
                std::path::Path::new(&instance.project_path),
                progress_tx,
                &hook_env,
            ) {
                builder::cleanup_instance(
                    &instance,
                    created_worktree.as_ref(),
                    &created_workspace_worktrees,
                    None,
                );
                return CreationResult::Error(on_create_error(&e, hooks));
            }
        }

        // Execute on_launch hooks in background too (non-fatal, like start_with_size).
        // This prevents blocking the UI thread when the session is first attached.
        if has_on_launch {
            let hooks = hooks.as_ref().unwrap();
            if sandbox {
                if !container_started {
                    if let Err(e) = instance.get_container_for_instance() {
                        let msg = format!("Container startup warning: {:#}", e);
                        tracing::warn!(target: "session.create", "{}", msg);
                        let _ = progress_tx.send(HookProgress::Output(msg));
                    } else {
                        container_started = true;
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
            // Only ensure the container is running here if hooks didn't already
            // start it. Don't create the tmux session yet -- that happens at attach time
            // where the terminal size is available.
            if let Err(e) = instance.get_container_for_instance() {
                builder::cleanup_instance(
                    &instance,
                    created_worktree.as_ref(),
                    &created_workspace_worktrees,
                    None,
                );
                return CreationResult::Error(format!("{:#}", e));
            }
        }

        let created_worktree_info = created_worktree.as_ref().map(CreatedWorktreeInfo::from);
        let created_workspace_worktree_info = created_workspace_worktrees
            .iter()
            .map(CreatedWorktreeInfo::from)
            .collect();

        CreationResult::Success {
            session_id: instance.id.clone(),
            instance: Box::new(instance),
            created_worktree: created_worktree_info,
            created_workspace_worktrees: created_workspace_worktree_info,
            on_launch_hooks_ran: has_on_launch,
            warnings,
        }
    }

    pub fn request_creation(&mut self, request: CreationRequest) {
        self.pending = true;
        self.last_profile = Some(request.data.profile.clone());
        if self
            .request_tx
            .send((request, self.progress_tx.clone()))
            .is_err()
        {
            tracing::error!(target: "session.create", "Failed to send creation request: receiver thread died");
            self.pending = false;
        }
    }

    pub fn last_profile(&self) -> Option<String> {
        self.last_profile.clone()
    }

    pub fn try_recv_result(&mut self) -> Option<CreationResult> {
        match self.result_rx.try_recv() {
            Ok(result) => {
                self.pending = false;
                Some(result)
            }
            Err(_) => None,
        }
    }

    /// Blocking receive with timeout, used during shutdown cleanup.
    pub fn recv_result_timeout(&mut self, timeout: std::time::Duration) -> Option<CreationResult> {
        match self.result_rx.recv_timeout(timeout) {
            Ok(result) => {
                self.pending = false;
                Some(result)
            }
            Err(_) => None,
        }
    }

    pub fn try_recv_progress(&self) -> Option<HookProgress> {
        self.progress_rx.try_recv().ok()
    }

    pub fn is_pending(&self) -> bool {
        self.pending
    }
}

impl Default for CreationPoller {
    fn default() -> Self {
        Self::new()
    }
}
