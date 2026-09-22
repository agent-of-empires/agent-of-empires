//! Native session creation shared by HTTP and plugin callers.
//! Admitted work owns provisioning, canonical commits and launch completion.

use std::sync::Arc;

use crate::session::Instance;

use super::session_service::SessionService;

/// Validated creation inputs shared by HTTP and plugin callers.
pub(crate) struct StructuredSessionSpec {
    pub title: Option<String>,
    pub size: Option<crate::daemon::TerminalSize>,
    pub path: String,
    pub group: String,
    pub tool: String,
    pub worktree_enabled: bool,
    pub worktree_branch: Option<String>,
    pub create_new_branch: bool,
    pub base_branch: Option<String>,
    pub sandbox: bool,
    pub sandbox_image: Option<String>,
    pub yolo_mode: bool,
    pub extra_env: Vec<String>,
    pub extra_args: String,
    pub command_override: String,
    pub extra_repo_paths: Vec<String>,
    /// Per-repo creation bases as `(selector, base)` pairs. See #3329.
    pub repo_base_branches: Vec<(String, String)>,
    pub scratch: bool,
    pub trust_hooks: Option<bool>,
    pub trust_review: Option<crate::daemon::CreationTrustFingerprint>,
    pub custom_instruction: Option<String>,
    /// External work-queue dispatcher completion callback, persisted onto
    /// the created instance. See #3156.
    pub callback_url: Option<String>,
    /// Idempotency key, persisted onto the created instance so a retry (even
    /// across a daemon restart) can be matched back to it. See #3156.
    pub idempotency_key: Option<String>,
    /// Resolved source profile (request profile, else the server default).
    pub profile: String,
    /// Creating plugin id, when the caller is a plugin worker rather than a
    /// user surface. Stamped by `SessionService::create_structured_session`,
    /// never decoded from a request body. See #2897.
    pub created_by_plugin: Option<String>,
    /// Plugin create-idempotency record to persist with the instance,
    /// stamped alongside `created_by_plugin`.
    pub plugin_create_idempotency: Option<crate::session::PluginCreateIdempotency>,
    /// Initial prompt to persist with the instance and deliver once the ACP
    /// worker is live, stamped by `SessionService::create_structured_session`.
    pub pending_initial_turn: Option<String>,
    /// Explicit ACP approval-mode id to persist on the instance; the
    /// supervisor applies it after every worker (re)spawn. Stamped by the
    /// plugin host create path after host-side classification.
    pub acp_mode_id: Option<String>,
    pub view: crate::session::View,
    pub agent_name: Option<String>,
    pub agent_model: Option<String>,
    pub agent_effort: Option<String>,
    pub import_acp_session_id: Option<String>,
    pub fork_seed: Option<crate::session::ForkSeed>,
}

/// Created session and transient warnings; HTTP row authority is the runtime snapshot.
pub(crate) struct SpawnOutcome {
    pub instance: Instance,
    pub warnings: Vec<String>,
}

/// Marker error the core returns when the blocking build task panicked, so the
/// HTTP handler can keep answering `500 Internal Server Error` for that case
/// while a plain build failure stays `400`. Mirrors the existing
/// `HooksNeedTrust` downcast pattern in the handler.
#[derive(Debug)]
pub(crate) struct SessionBuildPanicked(pub String);

impl std::fmt::Display for SessionBuildPanicked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for SessionBuildPanicked {}

/// Build, persist, and register a session, spawning its ACP worker when the
/// resolved view is structured. Returns the created instance and any build
/// warnings; a build-time panic is surfaced as a [`SessionBuildPanicked`] error
/// and a repo-trust refusal propagates as-is so the caller can map it.
pub(crate) async fn spawn_structured_session(
    service: &Arc<SessionService>,
    spec: StructuredSessionSpec,
) -> anyhow::Result<SpawnOutcome> {
    let state = service.native_state()?;
    let worker_state = state.clone();

    let result = tokio::task::spawn_blocking(move || {
        use crate::daemon::CreationPhase;
        use crate::session::builder::{self, InstanceParams};
        use crate::session::{LifecycleOperation, SessionStore, Status};
        let runtime = tokio::runtime::Handle::current();
        let namespace = runtime.block_on(worker_state.profile_namespace.read());
        let mut native = super::session_store::NativeSessionStore::open(
            worker_state.clone(), &spec.profile, None,
        )?;
        let creation_profile = worker_state.session_service.claim_creation_profile(&spec.profile);
        let config = native.configuration(Some(&spec.profile))?;
        drop(namespace);

        let StructuredSessionSpec {
            title,
            size,
            path,
            group,
            tool,
            worktree_enabled,
            worktree_branch,
            create_new_branch,
            base_branch,
            sandbox,
            sandbox_image,
            yolo_mode,
            extra_env,
            extra_args,
            command_override,
            extra_repo_paths,
            repo_base_branches,
            scratch,
            trust_hooks,
            trust_review,
            custom_instruction,
            callback_url,
            idempotency_key,
            profile,
            created_by_plugin,
            plugin_create_idempotency,
            pending_initial_turn,
            acp_mode_id,
            view,
            agent_name,
            agent_model,
            agent_effort,
            import_acp_session_id,
            fork_seed,
        } = spec;

        let sandbox_image = sandbox_image.unwrap_or_else(|| {
            if config.sandbox.default_image.is_empty() {
                "ubuntu:latest".to_string()
            } else {
                config.sandbox.default_image.clone()
            }
        });


        let extra_repo_paths: Vec<String> = extra_repo_paths
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect();

        // Persist approval against the source repository before provisioning can run Git hooks.
        let original_path = path.clone();
        let hook_plan = crate::server::api::sessions::resolve_create_hook_plan(
            &profile,
            &config.hooks,
            std::path::Path::new(&original_path),
            scratch,
            trust_hooks,
            trust_review.as_ref(),
        )?;
        if let Some((hooks_hash, mcp_hash)) = &hook_plan.trust_write {
            crate::session::config::repo_config::trust_repo(
                std::path::Path::new(&original_path), hooks_hash.as_deref(), mcp_hash.as_deref(),
            )?;
        }

        let title = title.unwrap_or_default();
        let worktree_branch = worktree_branch
            .map(|b| b.trim().to_string())
            .filter(|b| !b.is_empty());

        let params = InstanceParams {
            title,
            path,
            group,
            tool,
            worktree_enabled,
            worktree_branch,
            create_new_branch,
            base_branch: if create_new_branch {
                base_branch
                    .as_ref()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
            } else {
                None
            },
            sandbox,
            sandbox_image,
            yolo_mode,
            extra_env,
            extra_args,
            command_override,
            extra_repo_paths,
            repo_base_branches: if create_new_branch {
                repo_base_branches
            } else {
                // The base only matters when aoe creates the branch, the same
                // gate `base_branch` above uses.
                Vec::new()
            },
            scratch,
            fork_seed,
        };

        let namespace = runtime.block_on(worker_state.profile_namespace.read());
        let identity = crate::session::acquire_session_identity_lock()?;
        let (existing_titles, existing_branches): (Vec<String>, Vec<String>) = {
            let rows = worker_state.instances.blocking_read();
            (rows.iter().map(|row| row.title.clone()).collect(),
             rows.iter().filter_map(|row| row.worktree_info.as_ref().map(|worktree| worktree.branch.clone())).collect())
        };
        let title_refs: Vec<&str> = existing_titles.iter().map(String::as_str).collect();
        let branch_refs: Vec<&str> = existing_branches.iter().map(String::as_str).collect();
        let mut plan = builder::plan_instance(params, &title_refs, &branch_refs, &profile)?;
        let instance = &mut plan.result.instance;
        instance.source_profile = profile.clone();
        instance.file_watch = Some(worker_state.file_watch.clone());
        native.set_status_id(instance.id.clone());
        instance.created_by_plugin = created_by_plugin;
        instance.plugin_create_idempotency = plugin_create_idempotency;
        instance.pending_initial_turn =
            pending_initial_turn.map(|text| crate::session::PendingInitialTurn {
                text,
                attachments: Vec::new(),
                synthesized: false,
            });
        instance.acp_mode_id = acp_mode_id;
        instance.callback_url = callback_url;
        instance.idempotency_key = idempotency_key;

        // Apply per-session sandbox overrides from the request body.
        if let Some(ref mut sandbox) = instance.sandbox_info {
            if custom_instruction.is_some() {
                sandbox.custom_instruction = custom_instruction;
            }
        }
        instance.view = view;
        // Imports resume the existing structured conversation.
        if let Some(import_id) = import_acp_session_id.filter(|id| !id.trim().is_empty()) {
            instance.view = crate::session::View::Structured;
            instance.acp_session_id = Some(import_id);
            instance.import_pending = Some(true);
        }
        instance.agent_name = agent_name;
        let lock = runtime.block_on(worker_state.instance_lock(&instance.id));
        let store: &dyn SessionStore = &native;
        let creation_guard = worker_state.session_service.register_creation(
            &instance.id,
            &instance.title,
            &instance.source_profile,
        )?;
        let creation_progress = |event| creation_guard.hook_event(event);
        let generation = {
            let _submission = runtime.block_on(worker_state.session_service.prompt_submission(&instance.id));
            let _guard = lock.blocking_lock();
            native.check_available()?;
            let _title = crate::session::acquire_session_title_lock(&instance.id)?;
            let _lifecycle = native.storage().acquire_instance_lifecycle_lock(&instance.id)?;
            let generation = instance.try_acquire_lifecycle_reservation(
                LifecycleOperation::Launch, Instance::LIFECYCLE_RESERVATION_TTL, chrono::Utc::now(),
            )?;
            instance.status = Status::Starting;
            store.update(|rows, groups| {
                anyhow::ensure!(!rows.iter().any(|row| row.id == instance.id), "created session already exists");
                rows.push(instance.clone());
                *groups = crate::session::GroupTree::new_with_groups(std::slice::from_ref(instance), groups).get_all_groups();
                Ok(())
            })?;
            generation
        };
        drop(identity);
        drop(namespace);

        creation_guard.phase(CreationPhase::Provisioning);
        let (build_result, provision_error) = match plan.provision() {
            Ok(result) => (result, None),
            Err(failure) => {
                let failure = *failure;
                (failure.result, Some(failure.error))
            }
        };
        let mut instance = build_result.instance;
        let build_warnings = build_result.warnings;
        let created_worktree = build_result.created_worktree;
        let created_workspace_worktrees = build_result.created_workspace_worktrees;
        let agent_effort = if provision_error.is_none() {
            let resolved_config = crate::session::config::repo_config::resolve_config_with_repo_or_warn(
                &instance.source_profile,
                std::path::Path::new(&instance.project_path),
            );
            let acp_registry = crate::acp::AgentRegistry::with_defaults();
            // The defaults, and the pin, are keyed by the agent the spawn
            // runs, resolved the way the supervisor resolves it.
            let agent_key = crate::acp::pick_acp_agent_name(
                &acp_registry,
                &resolved_config.session,
                &resolved_config.acp,
                &instance.tool,
                instance.agent_name.as_deref(),
            );
            let defaults = resolved_config.acp.acp_defaults_for(&agent_key);
            // Preserve the explicit request model separately (trimmed to match
            // the resolver's normalization) so a terminal fallback below can
            // keep it while dropping any ACP-derived default; agent_model is
            // ACP-only.
            let explicit_model = agent_model
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            // A profile pin wins, else the explicit request, else the per-agent
            // default; effort is keyed on the resolved model. Same single-source
            // resolver the spawn path uses; persist the model here so the
            // composer shows it and the session stays on it. See
            // resolve_spawn_model_effort.
            // Persist only an EXPLICIT effort, never the resolved default:
            // `acp_effort` is a pin, and `None` means "inherit whatever the
            // configured default resolves to at spawn time". Snapshotting the
            // default here would freeze the session on today's value and make a
            // later config change invisible to it. The resolved effort still
            // reaches this session's first spawn below.
            let explicit_effort = agent_effort
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let (resolved_model, mut agent_effort) =
                crate::session::config::resolve_spawn_model_effort(
                    defaults,
                    explicit_model.clone(),
                    agent_effort,
                );
            instance.agent_model = resolved_model;
            instance.acp_effort = explicit_effort;
            // Don't trust the client's capability decision. Re-resolve
            // whether this agent can actually run in structured view; a custom
            // agent without an `agent_acp_cmd` (or any non-ACP tool)
            // falls back to tmux here rather than erroring at spawn time.
            if instance.is_structured() {
                let resolved = instance
                    .agent_name
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .unwrap_or(instance.tool.as_str());
                let resolved_session = &resolved_config.session;
                // Check the resolved agent key AND the raw tool, the same pair
                // `aoe add`'s precondition uses. Checking only `tool` for the
                // `agent_acp_cmd` / inheritance legs downgraded a session that
                // `agent_is_acp_capable` had already accepted as Structured,
                // whenever `agent_name` differed from `tool` (a custom agent,
                // or a wrapper inheriting a registry base), and the downgrade
                // also cleared its pending markers.
                let acp_capable_key = |key: &str| {
                    acp_registry.get(key).is_some()
                        || resolved_session
                            .agent_acp_cmd
                            .get(key)
                            .is_some_and(|cmd| crate::acp::AgentSpec::from_acp_cmd(key, cmd).is_ok())
                        // A custom agent that inherits a registry-backed base
                        // (e.g. a Claude wrapper) is structured-capable through
                        // the base adapter; keep the requested Structured view.
                        || crate::acp::inherited_acp_base(key, &resolved_session.agent_detect_as)
                            .is_some()
                };
                let capable = acp_capable_key(resolved) || acp_capable_key(&instance.tool);
                if capable {
                    instance.view = crate::session::View::Structured;
                } else {
                    instance.view = crate::session::View::Terminal;
                    // A non-ACP tool cannot run the structured session/fork
                    // handshake. If a malformed request seeded a structured
                    // fork (fork_pending/import_pending set by the builder),
                    // drop those markers so a later switch-to-structured does
                    // not fire an unexpected session/fork against the parent.
                    instance.fork_pending = None;
                    instance.import_pending = None;
                }
            }

            if !instance.is_structured() {
                agent_effort = None;
                // Terminal sessions keep only an explicitly requested model,
                // never an ACP-derived default (agent_model is ACP-only).
                instance.agent_model = explicit_model;
                // acp_effort is ACP-only too: nothing applies it in tmux mode.
                instance.acp_effort = None;
            }

            agent_effort
        } else {
            None
        };

        {
            let _namespace = runtime.block_on(worker_state.profile_namespace.read());
            let _submission = runtime.block_on(worker_state.session_service.prompt_submission(&instance.id));
            let _guard = lock.blocking_lock();
            store.update(|rows, _| {
                let row = rows.iter_mut().find(|row| row.id == instance.id)
                    .ok_or(crate::session::LifecycleReservationError::Superseded)?;
                anyhow::ensure!(row.lifecycle_reservation_is_owned(LifecycleOperation::Launch, generation)
                    && row.project_path == instance.project_path,
                    crate::session::LifecycleReservationError::Superseded);
                match (&mut row.worktree_info, &instance.worktree_info) {
                    (Some(stored), Some(built)) => {
                        anyhow::ensure!(stored.branch == built.branch && stored.main_repo_path == built.main_repo_path,
                            crate::session::LifecycleReservationError::Superseded);
                        stored.managed_by_aoe = built.managed_by_aoe;
                    }
                    (None, None) => {}
                    _ => return Err(crate::session::LifecycleReservationError::Superseded.into()),
                }
                match (&mut row.workspace_info, &instance.workspace_info) {
                    (Some(stored), Some(built)) => {
                        anyhow::ensure!(stored.workspace_dir == built.workspace_dir && stored.repos.len() == built.repos.len(),
                            crate::session::LifecycleReservationError::Superseded);
                        for (stored_repo, built_repo) in stored.repos.iter_mut().zip(&built.repos) {
                            anyhow::ensure!(stored_repo.worktree_path == built_repo.worktree_path
                                && stored_repo.main_repo_path == built_repo.main_repo_path && stored_repo.branch == built_repo.branch,
                                crate::session::LifecycleReservationError::Superseded);
                            stored_repo.managed_by_aoe = built_repo.managed_by_aoe;
                            stored_repo.branch_preexisting = built_repo.branch_preexisting;
                        }
                        stored.cleanup_on_delete = built.cleanup_on_delete;
                    }
                    (None, None) => {}
                    _ => return Err(crate::session::LifecycleReservationError::Superseded.into()),
                }
                row.view = instance.view;
                row.agent_model = instance.agent_model.clone();
                row.acp_effort = instance.acp_effort.clone();
                row.fork_pending = instance.fork_pending.clone();
                row.import_pending = instance.import_pending;
                instance = row.clone();
                Ok(())
            })?;
        }
        let provisioning_failed = provision_error.is_some();
        let creation = match provision_error {
            Some(error) => Err(error),
            None => creation_guard
                .check()
                .map_err(anyhow::Error::from)
                .and_then(|()| {
                    creation_guard.phase(CreationPhase::CreateHooks);
                    crate::server::api::sessions::run_create_hooks(
                        &mut instance,
                        &hook_plan,
                        store,
                        Some(&creation_progress),
                    )
                    .map_err(|error| {
                        let hint = hook_plan
                            .hooks
                            .as_ref()
                            .and_then(|hooks| hooks.origin_hint("on_create"));
                        anyhow::Error::new(
                            crate::server::api::sessions::CreateHookFailed::new(
                                error,
                                hint.as_deref(),
                            ),
                        )
                    })
                })
                .and_then(|()| creation_guard.check().map_err(anyhow::Error::from)),
        };
        let rollback = |native: super::session_store::NativeSessionStore, instance: Instance, provisioning_failed: bool| { use crate::session::deletion::{DeletionDisposition, DeletionRequest, PurgeReservation, PurgeTransaction};
        let _namespace = runtime.block_on(worker_state.profile_namespace.read());
        let _submission = runtime.block_on(worker_state.session_service.prompt_submission(&instance.id));
        let _guard = lock.blocking_lock();
        let request = DeletionRequest {
            session_id: instance.id.clone(),
            delete_worktree: created_worktree.as_ref().is_some_and(|worktree| worktree.checkout_created)
                || created_workspace_worktrees.iter().any(|worktree| worktree.checkout_created)
                || instance.workspace_info.as_ref().is_some_and(|workspace| workspace.cleanup_on_delete),
            delete_branch: created_worktree.as_ref().is_some_and(|worktree| worktree.owned_branch.is_some())
                || created_workspace_worktrees.iter().any(|worktree| worktree.owned_branch.is_some()),
            delete_sandbox: instance.sandbox_info.as_ref().is_some_and(|sandbox| sandbox.enabled),
            force_delete: true,
            detach_hooks: true,
            keep_scratch: provisioning_failed,
            instance,
        };
        let rollback = PurgeTransaction::reserve_failed_creation(native, request, generation)
            .map(|reservation| match reservation {
                PurgeReservation::Reserved(transaction) => transaction.complete_creation_rollback(
                    created_worktree.iter().chain(&created_workspace_worktrees),
                ),
                PurgeReservation::Rejected(result) => result,
            });
        match rollback {
            Ok(result) => {
                if matches!(result.disposition, DeletionDisposition::Removed | DeletionDisposition::AlreadyGone) {
                    worker_state.instance_locks.blocking_write().remove(&result.session_id);
                    runtime.block_on(worker_state.session_service.forget_prompt_lock(&result.session_id));
                }
                if !result.success {
                    tracing::warn!(target: "session.create", errors = ?result.errors, "Creation rollback incomplete");
                }
            }
            Err(rollback_error) => tracing::warn!(target: "session.create", %rollback_error, "Creation rollback could not acquire ownership"),
        } }; if let Err(error) = creation { rollback(native, instance, provisioning_failed); return Err(error); }

        let namespace = runtime.block_on(worker_state.profile_namespace.read());
        let submission = runtime.block_on(worker_state.session_service.prompt_submission(&instance.id));
        let guard = lock.blocking_lock();
        native.check_available()?;
        let title_lock = crate::session::acquire_session_title_lock(&instance.id)?;
        let lifecycle_lock = native.storage().acquire_instance_lifecycle_lock(&instance.id)?;
        instance.prepare_reserved_launch_hooks(store, false, crate::session::LaunchReservation {
            generation, title_lock, lifecycle_lock,
        })?;
        drop(guard);
        drop(submission);
        drop(namespace);

        creation_guard.phase(CreationPhase::LaunchHooks);
        let hooks = instance.run_pre_launch_hooks(false, store, Some(&creation_progress));
        if let Err(error) = creation_guard.commit() {
            rollback(native, instance, false);
            return Err(error.into());
        }

        let _namespace = runtime.block_on(worker_state.profile_namespace.read());
        let _submission = runtime.block_on(worker_state.session_service.prompt_submission(&instance.id));
        let _guard = lock.blocking_lock();
        if instance.is_structured() {
            let _ownership = instance.reacquire_launch_locks_after_hooks(store, generation, hooks)?;
            store.update(|rows, _| {
                let row = rows.iter_mut().find(|row| row.id == instance.id)
                    .ok_or(crate::session::LifecycleReservationError::Superseded)?;
                anyhow::ensure!(row.lifecycle_reservation_is_owned(LifecycleOperation::Launch, generation),
                    crate::session::LifecycleReservationError::Superseded);
                row.sandbox_info = instance.sandbox_info.clone();
                Ok(())
            })?;
            native.adopt_runtime_fields(&instance)?;
        } else {
            let outcome = instance.finish_reserved_launch(
                store,
                size.map(|size| (size.cols.get(), size.rows.get())),
                crate::session::ResumeLaunchOptions {
                    resume_policy: crate::session::ResumeAttemptPolicy::HonorAutoResumeSetting,
                    restart: false,
                    conversation_carry: None,
                },
                generation,
                hooks,
            );
            let _title = crate::session::acquire_session_title_lock(&instance.id)?;
            let _lifecycle = native.storage().acquire_instance_lifecycle_lock(&instance.id)?;
            native.adopt_runtime_fields(&instance)?;
            native.refresh_pane_observations(&instance)?;
            outcome?;
        }

        Ok::<_, anyhow::Error>((
            instance,
            build_warnings,
            agent_effort,
            native,
            creation_profile,
        ))
    })
    .await;

    match result {
        Ok(Ok((instance, warnings, agent_effort, native, creation_profile))) => {
            let acp_spawn_target = if instance.is_structured() {
                Some((
                    instance.id.clone(),
                    instance.tool.clone(),
                    instance.agent_name.clone(),
                    instance.agent_model.clone(),
                    agent_effort,
                    instance.acp_effort.is_some(),
                    instance.project_path.clone(),
                    instance.acp_session_id.clone(),
                    instance.source_profile.clone(),
                    instance.yolo_mode,
                    instance.acp_mode_id.clone(),
                    instance.command.clone(),
                    instance.import_pending == Some(true),
                    instance.fork_pending.clone(),
                ))
            } else {
                None
            };
            let response_instance = instance.clone();

            // Count the create for the opt-in telemetry trend counter. Bounded
            // accumulator, read-and-decremented by the snapshot loop; no-op for
            // opted-out installs (the snapshot is never built / sent).
            service
                .telemetry_session_creates
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

            if let Some((
                id,
                tool,
                agent_override,
                model,
                effort,
                effort_explicit,
                project_path,
                stored_acp_session_id,
                source_profile,
                yolo_mode,
                acp_mode_id,
                command,
                seed_history_replay,
                fork_from,
            )) = acp_spawn_target
            {
                let agent = service
                    .acp_supervisor
                    .pick_agent_for_tool(
                        &tool,
                        agent_override.as_deref(),
                        &source_profile,
                        std::path::Path::new(&project_path),
                    )
                    .await;
                let command_override =
                    crate::server::acp_reconciler::command_override_for_spawn(&tool, &command);
                let cwd = std::path::PathBuf::from(project_path);
                let supervisor = service.acp_supervisor.clone();
                let service_for_check = service.clone();
                let has_pending_initial_turn = {
                    let instances = service.instances.read().await;
                    instances
                        .iter()
                        .find(|i| i.id == id)
                        .is_some_and(|i| i.pending_initial_turn.is_some())
                };
                let sandbox_info = response_instance.sandbox_info.clone();
                let generation = response_instance.lifecycle_generation;
                let spawn_state = state.clone();
                service.work.spawn("acp.create_spawn", async move {
                    let namespace = spawn_state.profile_namespace.read().await;
                    let submission = service_for_check.prompt_submission(&id).await;
                    let inst_lock = service_for_check.instance_lock(&id).await;
                    let guard = inst_lock.lock().await;
                    let check_id = id.clone();
                    let checked = tokio::task::spawn_blocking(move || {
                        use crate::session::{LifecycleOperation, SessionStore};
                        let validation = (|| -> anyhow::Result<()> {
                            let _title = crate::session::acquire_session_title_lock(&check_id)?;
                            let _lifecycle = native
                                .storage()
                                .acquire_instance_lifecycle_lock(&check_id)?;
                            (&native as &dyn SessionStore).launch_configuration(&cwd)?;
                            let rows = native.load()?;
                            anyhow::ensure!(
                                rows.iter().any(|row| row.id == check_id
                                    && row.lifecycle_reservation_is_owned(
                                        LifecycleOperation::Launch,
                                        generation
                                    )),
                                crate::session::LifecycleReservationError::Superseded
                            );
                            Ok(())
                        })();
                        (native, cwd, validation)
                    })
                    .await;
                    drop(guard);
                    drop(submission);
                    drop(namespace);
                    let (native, cwd, validation) = match checked {
                        Ok(checked) => checked,
                        Err(error) => {
                            supervisor.publish_startup_error(
                                &id,
                                format!("creation admission failed: {error}"),
                            );
                            return;
                        }
                    };
                    let source_profile_for_spawn = Some(source_profile);
                    let spawned = match validation {
                        Err(error) => Err(error),
                        Ok(()) => supervisor
                            .spawn(crate::acp::supervisor::SpawnRequest {
                                session_id: id.clone(),
                                agent: agent.clone(),
                                tool,
                                cwd,
                                additional_dirs: vec![],
                                provider_env: vec![],
                                model,
                                effort,
                                effort_explicit,
                                stored_acp_session_id,
                                fork_from,
                                sandbox_info,
                                source_profile: source_profile_for_spawn,
                                yolo_mode,
                                acp_mode_id,
                                agent_command_override: command_override,
                                seed_history_replay,
                            })
                            .await
                            .map_err(|error| {
                                anyhow::anyhow!(crate::server::api::structured_spawn_error_message(
                                    &error, &agent
                                ))
                            }),
                    };
                    if let Err(error) = &spawned {
                        tracing::warn!(target: "acp.supervisor", session = %id,
                            "auto-spawn after create failed: {error}");
                        supervisor.publish_startup_error(&id, error.to_string());
                    }
                    let settled = finish_created_structured_session(
                        &spawn_state,
                        native,
                        &id,
                        generation,
                        spawned,
                    )
                    .await;
                    drop(creation_profile);
                    match settled {
                        Ok(()) if has_pending_initial_turn => {
                            service_for_check.drain_pending_initial_turn(&id).await;
                        }
                        Ok(()) => {}
                        Err(error) => tracing::warn!(target: "acp.supervisor", session = %id,
                            "created session completion failed: {error}"),
                    }
                });
            }

            Ok(SpawnOutcome {
                instance: response_instance,
                warnings,
            })
        }
        Ok(Err(e)) => Err(e),
        Err(e) => Err(anyhow::Error::new(SessionBuildPanicked(e.to_string()))),
    }
}

async fn finish_created_structured_session(
    state: &Arc<super::AppState>,
    native: super::session_store::NativeSessionStore,
    id: &str,
    generation: u64,
    outcome: anyhow::Result<()>,
) -> anyhow::Result<()> {
    let namespace = state.profile_namespace.read().await;
    let submission = state.session_service.prompt_submission(id).await;
    let lock = state.instance_lock(id).await;
    let guard = lock.lock().await;
    let mut instance = state
        .instances
        .read()
        .await
        .iter()
        .find(|row| row.id == id)
        .cloned()
        .ok_or(crate::session::LifecycleReservationError::Superseded)?;
    let result = tokio::task::spawn_blocking(move || {
        let ownership = instance.reacquire_launch_locks_after_hooks(&native, generation, outcome);
        if ownership.is_err() {
            native.adopt_runtime_fields(&instance)?;
            return ownership.map(|_| ());
        }
        instance.status = crate::session::Status::Idle;
        let result = instance.commit_lifecycle_launch(&native, generation, false);
        native.adopt_runtime_fields(&instance)?;
        result
    })
    .await
    .map_err(|error| anyhow::anyhow!("structured creation completion failed: {error}"))?;
    drop(guard);
    drop(submission);
    drop(namespace);
    state.runtime.publish(state).await?;
    result
}
