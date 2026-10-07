//! Background session creation handler for TUI responsiveness
//!
//! This handles the potentially slow Docker operations (image pull, container creation)
//! in a background thread so the UI remains responsive.

use std::sync::mpsc;
use std::thread;

use tokio_util::sync::CancellationToken;

use crate::session::builder::{self, InstanceParams};
use crate::session::config::repo_config::{self, HookProgress, ResolvedHooks};
use crate::session::Instance;
use crate::tui::dialogs::NewSessionData;

pub struct CreationRequest {
    pub storage: std::sync::Arc<crate::session::Storage>,
    pub admitted_instance: Instance,
    pub data: NewSessionData,
    pub existing_instances: Vec<Instance>,
    /// Trusted hooks to execute after instance creation (already approved by user).
    pub hooks: Option<ResolvedHooks>,
    pub cancel: CancellationToken,
}

#[derive(Debug)]
pub enum CreationResult {
    Success {
        session_id: String,
        instance: Box<Instance>,
        creation_intent: std::sync::Arc<builder::CreationIntent>,
        on_launch_hooks_ran: bool,
        /// Non-fatal warnings from worktree creation (e.g. post-checkout hook
        /// failures). Surfaced as a transient toast in the UI.
        warnings: Vec<String>,
    },
    Error(String),
    /// Cancelled before publication; unproven ownership remains durable.
    Cancelled,
}

pub struct CreationOutcome {
    pub storage: std::sync::Arc<crate::session::Storage>,
    pub result: CreationResult,
    /// Cancellation raced the worker result; unpublished ownership remains retained.
    pub cancelled: bool,
}

pub struct CreationPoller {
    request_tx: mpsc::Sender<(CreationRequest, mpsc::Sender<HookProgress>)>,
    result_rx: mpsc::Receiver<(
        CreationResult,
        CancellationToken,
        std::sync::Arc<crate::session::Storage>,
    )>,
    progress_rx: mpsc::Receiver<HookProgress>,
    progress_tx: mpsc::Sender<HookProgress>,
    _handle: thread::JoinHandle<()>,
    /// Requests sent and not yet received, including cancelled ones still winding down.
    in_flight: usize,
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
        let (result_tx, result_rx) = mpsc::channel::<(
            CreationResult,
            CancellationToken,
            std::sync::Arc<crate::session::Storage>,
        )>();
        let (progress_tx, progress_rx) = mpsc::channel::<HookProgress>();

        let handle = thread::spawn(move || {
            while let Ok((request, prog_tx)) = request_rx.recv() {
                let cancel = request.cancel.clone();
                let storage = std::sync::Arc::clone(&request.storage);
                let result = Self::create_instance(request, &prog_tx);
                if let Err(undelivered) = result_tx.send((result, cancel, storage)) {
                    if let CreationResult::Success { instance, .. } = &undelivered.0 .0 {
                        tracing::warn!(target: "tui.create", session_id = %instance.id, "Creation receiver closed; durable ownership and resources remain retained");
                    }
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
            in_flight: 0,
        }
    }

    fn create_instance(
        request: CreationRequest,
        progress_tx: &mpsc::Sender<HookProgress>,
    ) -> CreationResult {
        if let Err(error) = request.storage.verify_profile_identity() {
            return CreationResult::Error(format!("Creation profile was replaced: {error:#}"));
        }
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

        let build_result = match builder::build_instance_from_admitted(
            params,
            &existing_titles,
            &existing_branches,
            request.admitted_instance,
            &request.storage,
        ) {
            Ok(r) => r,
            Err(e) => return CreationResult::Error(format!("{:#}", e)),
        };

        let mut instance = build_result.instance;
        // Resolve profile-specific hooks against the original admitted profile.
        instance.source_profile = profile.clone();
        if structured {
            builder::structured::apply_structured_choice(&mut instance);
        }
        let warnings = build_result.warnings;
        let creation_intent = build_result.creation_intent;
        if let Err(error) = creation_intent.refresh_prepared(&instance) {
            return CreationResult::Error(format!(
                "Creation intent changed; retaining resources: {error:#}"
            ));
        }
        let cancelled = |instance: &Instance| {
            CreationResult::Error(format!("Creation cancelled; session {} and its resources at {} are retained because original owner quiescence is unproven.", instance.id, instance.project_path))
        };
        let failed = |instance: &Instance, message: String| {
            if cancel.is_cancelled() {
                return cancelled(instance);
            }
            CreationResult::Error(format!("{message}\nSession {} and its resources at {} are retained because original owner quiescence is unproven.", instance.id, instance.project_path))
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

        if let Err(error) = request.storage.verify_profile_identity() {
            return CreationResult::Error(format!(
                "Creation profile was replaced; retaining resources: {error:#}"
            ));
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
            // Only ensure the container is running here if hooks didn't already
            // start it. Don't create the tmux session yet -- that happens at attach time
            // where the terminal size is available.
            if let Err(e) = instance.get_container_until_cancelled(&cancel) {
                return failed(&instance, format!("{:#}", e));
            }
        }
        if cancel.is_cancelled() {
            return cancelled(&instance);
        }

        let workspace_claim_lock = match crate::session::acquire_session_workspace_claim_lock() {
            Ok(lock) => lock,
            Err(error) => {
                tracing::warn!(target: "session.create", "Creation ownership and resources retained: original native quiescence is unproven");
                return CreationResult::Error(format!("{error:#}"));
            }
        };
        match crate::session::acquire_session_identity_lock() {
            Ok(lock) => {
                let locks = builder::CleanupOwnershipLocks::from_held(workspace_claim_lock, lock);
                if let Err(error) = request.storage.verify_profile_identity() {
                    return CreationResult::Error(format!(
                        "Creation profile was replaced; retaining resources: {error:#}"
                    ));
                }
                if let Err(error) = crate::session::validate_managed_workspace(&instance) {
                    tracing::warn!(target: "session.create", "Creation ownership and resources retained: original native quiescence is unproven");
                    return CreationResult::Error(format!(
                        "Managed workspace validation failed before persistence: {error}"
                    ));
                }
                // The ownership flocks are released before the result crosses
                // the channel. Carrying them would have the worker thread hold
                // global flocks the UI thread cannot take, and
                // `HomeView::apply_creation_results` re-validates and persists
                // under its own window, so no peer claim can slip in between.
                drop(locks);
            }
            Err(error) => {
                drop(workspace_claim_lock);
                tracing::warn!(target: "session.create", "Creation ownership and resources retained: original native quiescence is unproven");
                return CreationResult::Error(format!("{error:#}"));
            }
        }

        CreationResult::Success {
            session_id: instance.id.clone(),
            instance: Box::new(instance),
            creation_intent,
            on_launch_hooks_ran: has_on_launch,
            warnings,
        }
    }

    pub fn request_creation(&mut self, request: CreationRequest) {
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

    pub fn try_recv_result(&mut self) -> Option<CreationOutcome> {
        self.received(self.result_rx.try_recv().ok())
    }

    /// Receive a completed request within the shutdown deadline.
    pub fn recv_result_timeout(&mut self, timeout: std::time::Duration) -> Option<CreationOutcome> {
        self.received(self.result_rx.recv_timeout(timeout).ok())
    }

    fn received(
        &mut self,
        received: Option<(
            CreationResult,
            CancellationToken,
            std::sync::Arc<crate::session::Storage>,
        )>,
    ) -> Option<CreationOutcome> {
        let (result, cancel, storage) = received?;
        self.in_flight = self.in_flight.saturating_sub(1);
        Some(CreationOutcome {
            storage,
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
