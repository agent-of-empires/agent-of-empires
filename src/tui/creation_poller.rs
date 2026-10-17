//! Owned background session provisioning and rollback.

use std::sync::mpsc;
use std::thread;

use tokio_util::sync::CancellationToken;

use crate::session::builder::{self, InstanceParams};
use crate::session::config::repo_config::{self, HookProgress, ResolvedHooks};
use crate::session::Instance;
use crate::tui::dialogs::NewSessionData;

pub struct CreationRequest {
    pub data: NewSessionData,
    pub existing_instances: Vec<Instance>,
    /// Trusted hooks to execute after instance creation (already approved by user).
    pub hooks: Option<ResolvedHooks>,
    pub cancel: CancellationToken,
    pub provisional_creation: Option<std::sync::Arc<builder::ProvisionalCreation>>,
}

pub struct CreationSuccess {
    pub build_result: builder::BuildResult,
    pub on_launch_hooks_ran: bool,
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
                .field("session_id", &success.build_result.instance.id)
                .field("profile", &success.build_result.profile_storage.profile())
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
    pub provisional_creation: Option<std::sync::Arc<builder::ProvisionalCreation>>,
}

enum CreationCommand {
    Create(Box<CreationRequest>, mpsc::Sender<HookProgress>),
    Rollback {
        outcome: CreationOutcome,
        cause: Option<String>,
    },
}

pub struct CreationPoller {
    request_tx: mpsc::Sender<CreationCommand>,
    result_rx: mpsc::Receiver<(CreationOutcome, CancellationToken)>,
    progress_rx: mpsc::Receiver<HookProgress>,
    progress_tx: mpsc::Sender<HookProgress>,
    _handle: Option<thread::JoinHandle<()>>,
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
        Self::new_after(None, None, 0)
    }

    fn new_after(
        predecessor: Option<thread::JoinHandle<()>>,
        previous_results: Option<mpsc::Receiver<(CreationOutcome, CancellationToken)>>,
        in_flight: usize,
    ) -> Self {
        let (request_tx, request_rx) = mpsc::channel::<CreationCommand>();
        let (result_tx, result_rx) = mpsc::channel::<(CreationOutcome, CancellationToken)>();
        let (progress_tx, progress_rx) = mpsc::channel::<HookProgress>();
        #[cfg(test)]
        let result_queued_observer =
            std::sync::Arc::new(std::sync::Mutex::new(None::<mpsc::Sender<()>>));
        #[cfg(test)]
        let queued_observer = result_queued_observer.clone();

        let handle = thread::spawn(move || {
            if let Some(predecessor) = predecessor {
                let _ = predecessor.join();
            }
            if let Some(previous_results) = previous_results {
                while let Ok(received) = previous_results.recv() {
                    if let Err(error) = result_tx.send(received) {
                        if let CreationResult::Success(mut success) = error.0 .0.result {
                            if let Err(error) = success.build_result.rollback() {
                                tracing::error!(target: "session.create", "Disconnected predecessor rollback failed: {error:#}");
                            }
                        }
                    }
                }
            }
            while let Ok(command) = request_rx.recv() {
                let (request, prog_tx) = match command {
                    CreationCommand::Create(request, progress) => (*request, progress),
                    CreationCommand::Rollback { mut outcome, cause } => {
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            let CreationResult::Success(success) = &mut outcome.result else {
                                unreachable!("only unpublished success can require rollback");
                            };
                            match (success.build_result.rollback(), cause) {
                                (Ok(()), None) => CreationResult::Cancelled,
                                (Ok(()), Some(cause)) => CreationResult::Error(cause),
                                (Err(error), cause) => CreationResult::Error(format!(
                                    "{}Creation rollback could not finish: {error:#}",
                                    cause.map(|cause| format!("{cause}\n")).unwrap_or_default()
                                )),
                            }
                        }));
                        outcome.result = result.unwrap_or_else(|_| {
                            CreationResult::Error("Creation rollback worker panicked".into())
                        });
                        let cancel = CancellationToken::new();
                        if result_tx.send((outcome, cancel)).is_err() {
                            break;
                        }
                        #[cfg(test)]
                        if let Some(observer) = queued_observer.lock().unwrap().take() {
                            let _ = observer.send(());
                        }
                        continue;
                    }
                };
                let cancel = request.cancel.clone();
                let provisional_creation = request.provisional_creation.clone();
                let mut active = None;
                let result = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    Self::create_instance(request, &prog_tx, &mut active)
                })) {
                    Ok(result) => result,
                    Err(payload) => {
                        let detail = payload
                            .downcast_ref::<&str>()
                            .copied()
                            .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                            .unwrap_or("unknown panic payload");
                        let mut message = format!("Creation worker panicked: {detail}");
                        if let Some(transaction) = active {
                            if let Err(error) = transaction.rollback() {
                                message.push_str(&format!(
                                    "\nCreation rollback could not finish: {error:#}"
                                ));
                                let diagnostic = crate::session::storage::acquire_ownership_lock()
                                    .and_then(|root| {
                                        transaction
                                            .note_conflict_with_ownership(&root, message.clone())
                                    });
                                if let Err(error) = diagnostic {
                                    message.push_str(&format!(
                                        "\nConflict could not be recorded: {error:#}"
                                    ));
                                }
                            }
                        }
                        CreationResult::Error(message)
                    }
                };
                let outcome = CreationOutcome {
                    result,
                    cancelled: false,
                    provisional_creation,
                };
                if let Err(error) = result_tx.send((outcome, cancel)) {
                    if let CreationResult::Success(mut success) = error.0 .0.result {
                        if let Err(error) = success.build_result.rollback() {
                            tracing::error!(target: "session.create", "Disconnected creation rollback failed: {error:#}");
                        }
                    }
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
            _handle: Some(handle),
            in_flight,
            #[cfg(test)]
            result_queued_observer,
        }
    }

    fn restart_disconnected(&mut self) {
        let (_, closed_rx) = mpsc::channel();
        let previous_results = std::mem::replace(&mut self.result_rx, closed_rx);
        *self = Self::new_after(self._handle.take(), Some(previous_results), self.in_flight);
    }

    #[cfg(test)]
    pub(crate) fn finish_for_test(&mut self) -> thread::Result<()> {
        let (closed, _) = mpsc::channel();
        drop(std::mem::replace(&mut self.request_tx, closed));
        match self._handle.take() {
            Some(handle) => handle.join(),
            None => Ok(()),
        }
    }

    #[cfg(test)]
    pub(crate) fn disconnect_for_test(&mut self) {
        let (closed, _) = mpsc::channel();
        drop(std::mem::replace(&mut self.request_tx, closed));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while self
            ._handle
            .as_ref()
            .is_some_and(|handle| !handle.is_finished())
        {
            assert!(
                std::time::Instant::now() < deadline,
                "creation worker did not disconnect"
            );
            std::thread::yield_now();
        }
    }

    fn create_instance(
        request: CreationRequest,
        progress_tx: &mpsc::Sender<HookProgress>,
        active: &mut Option<std::sync::Arc<crate::session::claim::ResourceReservation>>,
    ) -> CreationResult {
        let data = request.data;
        let hooks = request.hooks;
        let cancel = request.cancel;
        if cancel.is_cancelled() {
            return CreationResult::Cancelled;
        }
        let profile = data.profile.clone();
        let sandbox = data.sandbox;
        let structured = data.structured;
        let has_on_create = hooks
            .as_ref()
            .is_some_and(|hooks| !hooks.hooks().on_create.is_empty());
        let has_on_launch = hooks
            .as_ref()
            .is_some_and(|hooks| !hooks.hooks().on_launch.is_empty());
        let existing_titles: Vec<&str> = request
            .existing_instances
            .iter()
            .map(|instance| instance.title.as_str())
            .collect();
        let existing_branches: Vec<&str> = request
            .existing_instances
            .iter()
            .filter_map(|instance| {
                instance
                    .worktree_info
                    .as_ref()
                    .map(|info| info.branch.as_str())
            })
            .collect();
        let mut params = InstanceParams::from(data);
        params.provisional_creation = request.provisional_creation;
        params.has_creation_callbacks = has_on_create || has_on_launch;
        let mut build_result =
            match builder::build_instance(params, &existing_titles, &existing_branches, &profile) {
                Ok(result) => result,
                Err(error) => return CreationResult::Error(format!("{error:#}")),
            };
        *active = build_result.transaction.clone();
        if structured {
            builder::structured::apply_structured_choice(&mut build_result.instance);
        }
        let outcome = build_result.run_on_create(|instance| {
            let checkpoint = || -> anyhow::Result<()> {
                anyhow::ensure!(!cancel.is_cancelled(), "Session creation cancelled");
                Ok(())
            };
            checkpoint()?;
            let hook_env = repo_config::lifecycle_env_vars(instance);
            let mut container_started = false;
            if has_on_create {
                let hooks = hooks.as_ref().expect("on_create plan exists");
                if sandbox {
                    instance.get_container_until_cancelled(&cancel)?;
                    container_started = true;
                    checkpoint()?;
                    if let Some(sandbox) = &instance.sandbox_info {
                        repo_config::execute_hooks_in_container_streamed(
                            &hooks.hooks().on_create, &sandbox.container_name,
                            &instance.container_workdir(), progress_tx, &hook_env)
                            .map_err(|error| anyhow::anyhow!(on_create_error(&error, hooks)))?;
                    }
                } else {
                    repo_config::execute_hooks_streamed(&hooks.hooks().on_create,
                        std::path::Path::new(&instance.project_path), progress_tx, &hook_env)
                        .map_err(|error| anyhow::anyhow!(on_create_error(&error, hooks)))?;
                }
            }
            if has_on_launch {
                checkpoint()?;
                let hooks = hooks.as_ref().expect("on_launch plan exists");
                if sandbox {
                    if !container_started {
                        match instance.get_container_until_cancelled(&cancel) {
                            Ok(_) => {
                                container_started = true;
                                checkpoint()?;
                            }
                            Err(error) => {
                                checkpoint()?;
                                let message = format!("Container startup warning: {error:#}");
                                tracing::warn!(target: "session.create", "{message}");
                                let _ = progress_tx.send(HookProgress::Output(message));
                            }
                        }
                    }
                    if container_started {
                        if let Some(sandbox) = &instance.sandbox_info {
                            if let Err(error) = repo_config::execute_hooks_in_container_streamed(
                                &hooks.hooks().on_launch, &sandbox.container_name,
                                &instance.container_workdir(), progress_tx, &hook_env)
                            {
                                tracing::warn!(target: "session.create", "on_launch hook failed in container: {error:#}");
                            }
                        }
                    }
                } else if let Err(error) = repo_config::execute_hooks_streamed(
                    &hooks.hooks().on_launch, std::path::Path::new(&instance.project_path),
                    progress_tx, &hook_env)
                {
                    tracing::warn!(target: "session.create", "on_launch hook failed: {error:#}");
                }
            }
            if sandbox && !container_started {
                checkpoint()?;
                instance.get_container_until_cancelled(&cancel)?;
            }
            checkpoint()
        });
        if let Err(error) = outcome {
            let cleanup = build_result.rollback();
            return match cleanup {
                Ok(()) if cancel.is_cancelled() => CreationResult::Cancelled,
                Ok(()) => CreationResult::Error(format!("{error:#}")),
                Err(cleanup) => CreationResult::Error(format!(
                    "{error:#}\nCreation rollback could not finish: {cleanup:#}"
                )),
            };
        }
        if cancel.is_cancelled() {
            return match build_result.rollback() {
                Ok(()) => CreationResult::Cancelled,
                Err(error) => {
                    CreationResult::Error(format!("Cancelled creation rollback failed: {error:#}"))
                }
            };
        }
        CreationResult::Success(Box::new(CreationSuccess {
            build_result,
            on_launch_hooks_ran: has_on_launch,
        }))
    }

    pub fn request_creation(&mut self, request: CreationRequest) -> anyhow::Result<()> {
        self.request_tx
            .send(CreationCommand::Create(
                Box::new(request),
                self.progress_tx.clone(),
            ))
            .map_err(|error| anyhow::anyhow!("Failed to send creation request: {error}"))?;
        self.in_flight += 1;
        Ok(())
    }

    pub fn request_rollback(
        &mut self,
        outcome: CreationOutcome,
        cause: Option<String>,
    ) -> Result<(), (CreationOutcome, Option<String>)> {
        let command = match self
            .request_tx
            .send(CreationCommand::Rollback { outcome, cause })
        {
            Ok(()) => {
                self.in_flight += 1;
                return Ok(());
            }
            Err(error) => error.0,
        };
        self.restart_disconnected();
        match self.request_tx.send(command) {
            Ok(()) => {
                self.in_flight += 1;
                Ok(())
            }
            Err(mpsc::SendError(CreationCommand::Rollback { outcome, cause })) => {
                Err((outcome, cause))
            }
            Err(_) => unreachable!("rollback send returned a different command"),
        }
    }

    #[cfg(test)]
    pub(crate) fn observe_next_queued_result(&self) -> mpsc::Receiver<()> {
        let (sender, receiver) = mpsc::channel();
        *self.result_queued_observer.lock().unwrap() = Some(sender);
        receiver
    }

    pub fn try_recv_result(&mut self) -> Result<CreationOutcome, mpsc::TryRecvError> {
        match self.result_rx.try_recv() {
            Ok(received) => Ok(self.received(received)),
            Err(error) => {
                if matches!(error, mpsc::TryRecvError::Disconnected) {
                    self.in_flight = 0;
                }
                Err(error)
            }
        }
    }

    /// Blocking receive with timeout, used during shutdown cleanup.
    pub fn recv_result_timeout(&mut self, timeout: std::time::Duration) -> Option<CreationOutcome> {
        match self.result_rx.recv_timeout(timeout) {
            Ok(received) => Some(self.received(received)),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                self.in_flight = 0;
                None
            }
            Err(mpsc::RecvTimeoutError::Timeout) => None,
        }
    }

    fn received(&mut self, received: (CreationOutcome, CancellationToken)) -> CreationOutcome {
        let (mut outcome, cancel) = received;
        self.in_flight = self.in_flight.saturating_sub(1);
        outcome.cancelled = cancel.is_cancelled();
        outcome
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
