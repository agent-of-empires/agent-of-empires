//! Domain core for creating a session: build the instance, persist it, upsert
//! it into the live state, and (for a structured session) spawn its ACP worker.
//!
//! Extracted from the `create_session` HTTP handler so a non-HTTP caller (for
//! example a scheduler) can create a structured-view ACP session without
//! duplicating the build/persist/spawn logic. The handler keeps all request
//! decoding, validation, and response construction; it decodes a request into a
//! [`StructuredSessionSpec`], calls [`spawn_structured_session`], and builds its
//! HTTP response from the returned [`SpawnOutcome`].

use std::sync::Arc;

use crate::session::Instance;

use super::session_service::SessionService;

/// Already-decoded, already-validated inputs the create core needs. The HTTP
/// handler fills this in after it has finished request parsing, auth, and
/// validation; a future non-HTTP caller builds it directly.
pub(crate) struct StructuredSessionSpec {
    pub title: Option<String>,
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
    pub scratch: bool,
    pub trust_hooks: Option<bool>,
    pub custom_instruction: Option<String>,
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
    #[cfg(feature = "serve")]
    pub view: crate::session::View,
    #[cfg(feature = "serve")]
    pub agent_name: Option<String>,
    #[cfg(feature = "serve")]
    pub agent_model: Option<String>,
    #[cfg(feature = "serve")]
    pub agent_effort: Option<String>,
    #[cfg(feature = "serve")]
    pub import_acp_session_id: Option<String>,
    #[cfg(feature = "serve")]
    pub fork_seed: Option<crate::session::ForkSeed>,
}

/// What the create core returns to its caller once the session exists in state.
/// The HTTP handler builds its `SessionResponse` from `instance` and threads
/// `warnings` onto it exactly as the inline handler did.
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
    let instances = service.instances.read().await;
    let existing_titles: Vec<String> = instances.iter().map(|i| i.title.clone()).collect();
    let existing_branches: Vec<String> = instances
        .iter()
        .filter_map(|i| i.worktree_info.as_ref().map(|w| w.branch.clone()))
        .collect();
    drop(instances);

    let file_watch_for_create = service.file_watch.clone();

    let result = tokio::task::spawn_blocking(move || {
        use crate::session::builder::{self, InstanceParams};
        use crate::session::Config;
        use crate::session::Storage;

        let StructuredSessionSpec {
            title,
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
            scratch,
            trust_hooks,
            custom_instruction,
            profile,
            created_by_plugin,
            plugin_create_idempotency,
            pending_initial_turn,
            acp_mode_id,
            #[cfg(feature = "serve")]
            view,
            #[cfg(feature = "serve")]
            agent_name,
            #[cfg(feature = "serve")]
            agent_model,
            #[cfg(feature = "serve")]
            agent_effort,
            #[cfg(feature = "serve")]
            import_acp_session_id,
            #[cfg(feature = "serve")]
            fork_seed,
        } = spec;

        let config = Config::load_or_warn();
        let sandbox_image = sandbox_image.unwrap_or_else(|| {
            if config.sandbox.default_image.is_empty() {
                "ubuntu:latest".to_string()
            } else {
                config.sandbox.default_image.clone()
            }
        });

        let title_refs: Vec<&str> = existing_titles.iter().map(|s| s.as_str()).collect();
        let branch_refs: Vec<&str> = existing_branches.iter().map(|s| s.as_str()).collect();
        let extra_repo_paths: Vec<String> = extra_repo_paths
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect();

        // Resolve repo hook trust BEFORE building the worktree (#2066): a repo
        // whose hooks need approval and that was not sent `trust_hooks: true`
        // is refused here, so the handler never leaves an orphan worktree on
        // disk. The original `path` is the trust anchor (the same source the
        // CLI/TUI use); `check_repo_trust` resolves a worktree path to its main
        // repo, so a worktree created from an already-trusted repo inherits its
        // trust without a separate prompt.
        let original_path = path.clone();
        let hook_plan = crate::server::api::sessions::resolve_create_hook_plan(
            &profile,
            std::path::Path::new(&original_path),
            scratch,
            trust_hooks.unwrap_or(false),
        )?;

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
            scratch,
            #[cfg(feature = "serve")]
            fork_seed,
            #[cfg(not(feature = "serve"))]
            fork_seed: None,
        };

        let build_result = builder::build_instance(params, &title_refs, &branch_refs, &profile)?;
        let mut instance = build_result.instance;
        instance.source_profile = profile.clone();
        instance.created_by_plugin = created_by_plugin;
        instance.plugin_create_idempotency = plugin_create_idempotency;
        instance.pending_initial_turn = pending_initial_turn;
        instance.acp_mode_id = acp_mode_id;
        let build_warnings = build_result.warnings;
        let created_worktree = build_result.created_worktree;
        let created_workspace_worktrees = build_result.created_workspace_worktrees;

        // Apply per-session sandbox overrides from the request body.
        if let Some(ref mut sandbox) = instance.sandbox_info {
            if custom_instruction.is_some() {
                sandbox.custom_instruction = custom_instruction;
            }
        }

        // Apply structured-view fields from the request body. structured_view is
        // re-validated below against real ACP capability; non-ACP tools
        // fall back to terminal view rather than erroring at spawn time.
        #[cfg(feature = "serve")]
        let agent_effort = {
            instance.view = view;
            // #2276: importing an existing Claude session forces the
            // structured view and adopts the on-disk session id, so the
            // structured spawn resumes it via session/load and seeds the
            // transcript from the agent's history replay. `path` is the
            // session's original cwd (the wizard prefills it).
            if let Some(import_id) = import_acp_session_id
                .clone()
                .filter(|s| !s.trim().is_empty())
            {
                instance.view = crate::session::View::Structured;
                instance.acp_session_id = Some(import_id);
                instance.import_pending = Some(true);
            }
            instance.agent_name = agent_name;
            let agent_key = instance
                .agent_name
                .as_deref()
                .filter(|s| !s.is_empty())
                .unwrap_or(instance.tool.as_str())
                .to_string();
            let resolved_config = crate::session::repo_config::resolve_config_with_repo_or_warn(
                &instance.source_profile,
                std::path::Path::new(&instance.project_path),
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
            // Explicit request wins, else the per-agent default; effort is keyed
            // on the resolved model. Same single-source resolver the spawn path
            // uses; persist the model here so the composer shows it and the
            // session stays pinned to it. See resolve_spawn_model_effort.
            let (resolved_model, mut agent_effort) =
                crate::session::config::resolve_spawn_model_effort(
                    defaults,
                    explicit_model.clone(),
                    agent_effort,
                );
            instance.agent_model = resolved_model;
            // Don't trust the client's capability decision. Re-resolve
            // whether this agent can actually run in structured view; a custom
            // agent without an `agent_acp_cmd` (or any non-ACP tool)
            // falls back to tmux here rather than erroring at spawn time.
            if instance.is_structured() {
                let acp_registry = crate::acp::AgentRegistry::with_defaults();
                let resolved = instance
                    .agent_name
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .unwrap_or(instance.tool.as_str());
                let capable = acp_registry.get(resolved).is_some()
                    || crate::session::repo_config::resolve_config_with_repo_or_warn(
                        &instance.source_profile,
                        std::path::Path::new(&instance.project_path),
                    )
                    .session
                    .agent_acp_cmd
                    .get(&instance.tool)
                    .is_some_and(|cmd| {
                        crate::acp::AgentSpec::from_acp_cmd(&instance.tool, cmd).is_ok()
                    });
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
            }

            agent_effort
        };

        // Run on_create hooks now that the worktree exists, before the session
        // is persisted or started (#2066). Mirrors the TUI/CLI ordering so the
        // worktree is bootstrapped (`.env` copies, venv symlinks, DB seeds)
        // before the agent launches. On failure, tear down the just-built
        // worktree/container so a broken hook doesn't leave an orphan.
        if let Err(e) = crate::server::api::sessions::run_create_hooks(
            &mut instance,
            &hook_plan,
            std::path::Path::new(&original_path),
        ) {
            builder::cleanup_instance(
                &instance,
                created_worktree.as_ref(),
                &created_workspace_worktrees,
            );
            return Err(anyhow::anyhow!("on_create hook failed: {e:#}"));
        }

        // Anything that fails between here and the final `Ok(..)`
        // would otherwise orphan the scratch directory `build_instance`
        // already provisioned (Storage::new, storage.update,
        // instance.start). Wrap the tail in an IIFE-equivalent closure
        // so we can run cleanup on Err once, regardless of which step
        // tripped. Matches the CLI cleanup path in
        // `cleanup_partial_session(... scratch_dir: Some(...))`.
        let mut persist_and_start = || -> anyhow::Result<()> {
            let storage = Storage::new(&profile, file_watch_for_create.clone())?;
            let to_persist = instance.clone();
            storage.update(|all, _groups| {
                all.push(to_persist);
                Ok(())
            })?;

            // Acp-mode sessions are not backed by tmux; the structured view
            // supervisor spawns the ACP agent on demand. Skip the tmux
            // `start()` to avoid creating an empty pane that no one will
            // attach to.
            #[cfg(feature = "serve")]
            let skip_tmux_start = instance.is_structured();
            #[cfg(not(feature = "serve"))]
            let skip_tmux_start = false;
            if !skip_tmux_start {
                instance.start()?;
            }
            Ok(())
        };

        if let Err(e) = persist_and_start() {
            // Guarded the same way as the deletion path: only remove a
            // path that `is_scratch_path` blesses, so a corrupted
            // `project_path` cannot trick us into wiping unrelated
            // state.
            if instance.scratch {
                let scratch_path = std::path::PathBuf::from(&instance.project_path);
                if crate::session::scratch::is_scratch_path(&scratch_path) {
                    if let Err(rm_err) = std::fs::remove_dir_all(&scratch_path) {
                        tracing::warn!(
                            target: "http.api.sessions",
                            "Failed to clean up orphan scratch dir {} after create failure: {}",
                            scratch_path.display(),
                            rm_err
                        );
                    }
                }
            }
            return Err(e);
        }

        #[cfg(feature = "serve")]
        return Ok::<(Instance, Vec<String>, Option<String>), anyhow::Error>((
            instance,
            build_warnings,
            agent_effort,
        ));

        #[cfg(not(feature = "serve"))]
        Ok::<(Instance, Vec<String>), anyhow::Error>((instance, build_warnings))
    })
    .await;

    match result {
        #[cfg(feature = "serve")]
        Ok(Ok((instance, warnings, agent_effort))) => {
            let response_instance = instance.clone();
            let acp_spawn_target = if instance.is_structured() {
                Some((
                    instance.id.clone(),
                    instance.tool.clone(),
                    instance.agent_name.clone(),
                    instance.agent_model.clone(),
                    agent_effort,
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
            let mut instances = service.instances.write().await;
            crate::server::api::sessions::upsert_instance(&mut instances, instance);
            drop(instances);

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
                tokio::spawn(async move {
                    let inst_lock = service_for_check.instance_lock(&id).await;
                    let sandbox_info = match crate::acp::sandbox::ensure_container_for_session(
                        &service_for_check.instances,
                        &inst_lock,
                        &id,
                        true,
                    )
                    .await
                    {
                        Ok(info) => info,
                        Err(e) => {
                            let message = format!("sandbox container ensure failed: {e}");
                            tracing::warn!(
                                target: "acp.supervisor",
                                session = %id,
                                "auto-spawn after create failed: {message}"
                            );
                            supervisor.publish_startup_error(&id, message);
                            return;
                        }
                    };
                    let source_profile_for_spawn = Some(source_profile.clone());
                    match supervisor
                        .spawn(crate::acp::supervisor::SpawnRequest {
                            session_id: id.clone(),
                            agent: agent.clone(),
                            cwd,
                            additional_dirs: vec![],
                            provider_env: vec![],
                            model,
                            effort,
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
                    {
                        Ok(()) => {
                            // Fast path for a create that carried an initial
                            // turn: deliver it now that the worker is live.
                            // The reconciler tick is the retry owner for
                            // every other case (spawn failure here, daemon
                            // restart, adopted runner).
                            if has_pending_initial_turn {
                                service_for_check.drain_pending_initial_turn(&id).await;
                            }
                        }
                        Err(e) => {
                            let still_present = service_for_check
                                .instances
                                .read()
                                .await
                                .iter()
                                .any(|i| i.id == id);
                            // Capacity-aware banner selection (and the benign
                            // first-tick duplicate) is documented on
                            // `structured_spawn_error_message`.
                            let message =
                                crate::server::api::structured_spawn_error_message(&e, &agent);
                            if still_present {
                                tracing::warn!(
                                    target: "acp.supervisor",
                                    session = %id,
                                    "auto-spawn after create failed: {message}"
                                );
                                supervisor.publish_startup_error(&id, message);
                            } else {
                                tracing::debug!(
                                    target: "acp.supervisor",
                                    session = %id,
                                    "auto-spawn after create error after session removed (ignored): {message}"
                                );
                            }
                        }
                    }
                });
            }

            Ok(SpawnOutcome {
                instance: response_instance,
                warnings,
            })
        }
        #[cfg(not(feature = "serve"))]
        Ok(Ok((instance, warnings))) => {
            let response_instance = instance.clone();
            let mut instances = service.instances.write().await;
            instances.push(instance);
            drop(instances);
            Ok(SpawnOutcome {
                instance: response_instance,
                warnings,
            })
        }
        Ok(Err(e)) => Err(e),
        Err(e) => Err(anyhow::Error::new(SessionBuildPanicked(e.to_string()))),
    }
}
