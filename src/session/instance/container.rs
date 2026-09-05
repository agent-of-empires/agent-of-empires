//! Container-backed sessions: config, workdir, and the environment minted
//! before start.

use super::*;

const IDENTITY_PUBLISHER_DEPENDENCY_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(2);

fn identity_publisher_dependencies_available(container: &containers::DockerContainer) -> bool {
    let check = vec![
        "sh".to_string(),
        "-c".to_string(),
        "command -v sh >/dev/null 2>&1 && command -v jq >/dev/null 2>&1".to_string(),
    ];
    let argv = container.build_exec_argv("", &check);
    let Some((program, args)) = argv.split_first() else {
        return false;
    };
    let mut command = std::process::Command::new(program);
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let available = matches!(
        crate::process::run_with_timeout(
            &mut command,
            IDENTITY_PUBLISHER_DEPENDENCY_TIMEOUT,
        ),
        Ok(Some(output)) if output.status.success()
    );
    if !available {
        tracing::warn!(
            target: "hooks.install",
            container = %container.name,
            "sandbox identity hooks need sh and jq; install both in the custom image to enable native session identity publication"
        );
    }
    available
}

fn identity_publisher_mount_matches(
    container: &containers::DockerContainer,
    config: &crate::containers::ContainerConfig,
) -> Result<bool> {
    Ok(container.mount_fingerprint_matches(config)? == Some(true))
}

impl Instance {
    /// Resolve the effective `environment` list for this session's profile,
    /// falling back to the global list when the profile has no override.
    pub(super) fn profile_host_environment(&self) -> Vec<String> {
        let profile = self.effective_profile();
        crate::session::config::profile_config::resolve_config_or_warn(&profile).environment
    }

    /// The host environment the agent process will actually see: the static
    /// profile `environment` list with every `before_session`-minted key
    /// dropped, then the minted pairs appended. This is the same precedence
    /// `build_launch_command` applies to the pane, so anything that has to
    /// agree with the launched agent about a variable's value must read it
    /// here rather than from `profile_host_environment` alone.
    ///
    /// Minted pairs are `#[serde(skip)]` runtime state, so outside a launch
    /// (a poller repair, a daemon-side read of a stored row) this degrades to
    /// the profile list. That is the best available answer: the minted values
    /// are deliberately not persisted because they may be short-lived secrets.
    pub(crate) fn resolved_host_environment(&self) -> Vec<String> {
        let mut environment = crate::session::environment::drop_shadowed_host_entries(
            self.profile_host_environment(),
            &self.pending_host_env,
        );
        environment.extend(self.pending_host_env.iter().map(|(key, value)| {
            // These are already-concrete hook values. Escape a leading `$`
            // back into the environment-list grammar so it remains literal.
            if value.starts_with('$') {
                format!("{key}=${value}")
            } else {
                format!("{key}={value}")
            }
        }));
        environment
    }

    pub fn get_container_for_instance(&mut self) -> Result<containers::DockerContainer> {
        let detect_as = self.effective_detect_as().into_owned();
        let image = self
            .sandbox_info
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Cannot ensure container for non-sandboxed session"))?
            .image
            .clone();
        let container = DockerContainer::new(&self.id, &image);
        let _transition_lock =
            if self.sandbox_store_generation < container_config::CURRENT_SANDBOX_STORE_GENERATION {
                Some(crate::session::acquire_storage_shared_flock(
                    &crate::session::get_app_dir()?,
                    crate::migrations::v027_isolate_sandbox_stores::LOCK,
                )?)
            } else {
                None
            };

        // Direct is_running()? / exists()? here rather than probe_running():
        // this function already returns Result, so `?` correctly propagates
        // a daemon-down transient to the caller as Err, letting them render
        // an actionable error rather than silently falling through to a
        // create attempt that would also fail. See #2596.
        if container.is_running()? {
            if self.sandbox_store_generation >= container_config::CURRENT_SANDBOX_STORE_GENERATION
                && container.sandbox_store_generation_matches()? == Some(false)
            {
                anyhow::bail!(
                    "running sandbox {} uses a legacy store generation; stop it before relaunch",
                    self.id
                );
            }
            // Already up: not a come-up, so don't re-mint. Fill lazily only if a
            // fresh process attached to a running container with no values yet.
            self.ensure_before_start_env(false)?;
            if self.sandbox_store_generation < container_config::CURRENT_SANDBOX_STORE_GENERATION {
                self.backfill_container_workdir(&container);
                return Ok(container);
            }
            container_config::refresh_agent_configs_for_instance(
                &self.effective_profile(),
                &self.id,
                &self.tool,
                Some(detect_as.as_str()),
            );
            let config = self.build_container_config()?;
            self.identity_publisher_launched = config.identity_publisher_installed
                && identity_publisher_mount_matches(&container, &config)?
                && identity_publisher_dependencies_available(&container)
                && self.hook_session_publisher_allowed_by_argv();
            self.backfill_container_workdir(&container);
            container_config::ensure_folder_trust_config_for_active_agent(
                &self.tool,
                Some(detect_as.as_str()),
                &self.source_profile,
                &self.id,
                &self.container_workdir(),
                self.is_yolo_mode(),
            );
            return Ok(container);
        }

        if self.sandbox_store_generation < container_config::CURRENT_SANDBOX_STORE_GENERATION {
            anyhow::bail!(
                "sandbox store transition is pending for {}; stop all legacy sandbox peers and restart AoE before relaunching it",
                self.id
            );
        }

        if container.exists()? {
            if container.sandbox_store_generation_matches()? == Some(false) {
                container.remove(false)?;
            } else {
                // Restart of a stopped container is a come-up: refresh so a
                // short-lived token is re-minted.
                self.ensure_before_start_env(true)?;
                container_config::refresh_agent_configs_for_instance(
                    &self.effective_profile(),
                    &self.id,
                    &self.tool,
                    Some(detect_as.as_str()),
                );
                let config = self.build_container_config()?;
                container.start()?;
                self.identity_publisher_launched = config.identity_publisher_installed
                    && identity_publisher_mount_matches(&container, &config)?
                    && identity_publisher_dependencies_available(&container)
                    && self.hook_session_publisher_allowed_by_argv();
                self.backfill_container_workdir(&container);
                container_config::ensure_folder_trust_config_for_active_agent(
                    &self.tool,
                    Some(detect_as.as_str()),
                    &self.source_profile,
                    &self.id,
                    &self.container_workdir(),
                    self.is_yolo_mode(),
                );
                return Ok(container);
            }
        }

        // Ensure image is available (always pulls to get latest)
        let runtime = containers::get_container_runtime();
        runtime.ensure_image(&image)?;

        // Mint before building the container config so the docker-run env also
        // carries the values (leak-safe via the inherit path in run_create).
        self.ensure_before_start_env(true)?;
        let config = self.build_container_config()?;
        // Still the workdir the *previous* container was created with; the pin below
        // is what moves it forward.
        let stranded = container_config::stranded_named_ignore_volumes(
            &config,
            &self.id,
            self.sandbox_info
                .as_ref()
                .and_then(|sandbox| sandbox.container_workdir.as_deref()),
        );
        container.remove_stranded_named_ignore_volumes(&self.id, &stranded);
        let container_id = container.create(&config)?;
        self.identity_publisher_launched = config.identity_publisher_installed
            && identity_publisher_dependencies_available(&container)
            && self.hook_session_publisher_allowed_by_argv();

        if let Some(ref mut sandbox) = self.sandbox_info {
            sandbox.container_id = Some(container_id);
            // Pin the workdir to exactly what the container was built with, so
            // later `docker exec -w` can never drift from it (#2414).
            sandbox.container_workdir = Some(config.working_dir.clone());
        }

        Ok(container)
    }

    /// Backfill [`SandboxInfo::container_workdir`] from a live container for a
    /// session created before that field existed (or one whose value was
    /// cleared). Authoritative: the value is the container's own
    /// `Config.WorkingDir`, so a later host-side git-linkage break can't make
    /// [`Self::container_workdir`] drift from the path the container was built
    /// with (#2414). No-op once the value is set, when the session is not
    /// sandboxed, or when the runtime can't report it (the live fallback
    /// stands). Not persisted here; the next start re-backfills if needed.
    fn backfill_container_workdir(&mut self, container: &containers::DockerContainer) {
        let needs_backfill = self
            .sandbox_info
            .as_ref()
            .is_some_and(|s| s.container_workdir.is_none());
        if !needs_backfill {
            return;
        }
        if let Some(workdir) = container.working_dir() {
            if let Some(sandbox) = self.sandbox_info.as_mut() {
                sandbox.container_workdir = Some(workdir);
            }
        }
    }

    /// Get the container working directory for this instance.
    /// The working directory a `docker exec` into this session's sandbox must
    /// chdir to. Pinned to what the container was actually created with
    /// ([`SandboxInfo::container_workdir`]): set at create time from
    /// `ContainerConfig::working_dir` and backfilled from a live container for
    /// sessions that predate the field.
    ///
    /// Recomputing it live from `compute_volume_paths` is unsafe, which is what
    /// #2414 hit: that helper resolves the worktree's git linkage, and once the
    /// container is up that linkage can break on the host (e.g. the worktree's
    /// admin entry under `<main>/.git/worktrees/<name>` is pruned). When it
    /// can't resolve, `compute_volume_paths` silently collapses to
    /// `/workspace/<basename>` -- a path the container never mounted -- and the
    /// exec dies with `chdir to cwd ("/workspace/<name>") ... no such file or
    /// directory`. The live computation survives only as a fallback for a
    /// session whose container has not been created yet, where there is nothing
    /// to pin to.
    pub fn container_workdir(&self) -> String {
        if let Some(pinned) = self
            .sandbox_info
            .as_ref()
            .and_then(|s| s.container_workdir.clone())
        {
            return pinned;
        }
        container_config::compute_volume_paths(Path::new(&self.project_path), &self.project_path)
            .map(|(_, wd)| wd)
            .unwrap_or_else(|_| "/workspace".to_string())
    }

    pub(super) fn build_container_config(&self) -> Result<crate::containers::ContainerConfig> {
        let detect_as = self.effective_detect_as();
        let sandbox = self
            .sandbox_info
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("sandbox_info missing for sandboxed session"))?;
        // Resolve the user-selected agent (e.g. Kiro `--agent NAME`) so the
        // sandbox installs status hooks into that agent's config, matching the
        // host path. Gated by the same setting; only applies to agents that
        // declare selected_agent_hooks.
        let merge_selected = crate::session::config::profile_config::resolve_config_or_warn(
            &self.effective_profile(),
        )
        .session
        .merge_hooks_into_selected_agent;
        let selected_agent = if merge_selected {
            // Mirror the host path's agent resolution (a custom wrapper detected
            // as kiro carries kiro's sidecar via detect_as), and the sandbox's
            // own `resolve_active_agent`, which also falls back to detect_as.
            self.resolved_agent()
                .and_then(|a| a.sidecar_hooks.as_ref())
                .and_then(|s| s.selected_agent_hooks.as_ref())
                .and_then(|sel| {
                    crate::agents::parse_selected_agent(&self.selected_agent_args(), sel.flag)
                })
        } else {
            None
        };
        container_config::build_container_config(
            &self.project_path,
            sandbox,
            container_config::ContainerAgentSelection::new(&self.tool, Some(&detect_as))
                .with_selected_agent(selected_agent.as_deref()),
            self.is_yolo_mode(),
            &self.id,
            self.workspace_info.as_ref(),
            &self.source_profile,
        )
    }

    /// Run `host_hooks.before_start` on the host and stash the resulting
    /// `KEY=VALUE` pairs on `sandbox_info.before_start_env`, from where
    /// [`crate::session::environment::collect_environment`] injects them into the
    /// container environment on every surface (docker run, the tmux `docker
    /// exec` launch, and the structured-view worker).
    ///
    /// `force` re-mints unconditionally (a container come-up); when false the
    /// hooks run only if no values are stashed yet, so attaching to an
    /// already-running container backfills without re-minting on every relaunch.
    /// A hook failure is propagated so the container does not come up without
    /// the values the agent depends on. Hooks are resolved from profile/global
    /// config only, never from the repo.
    pub(super) fn ensure_before_start_env(&mut self, force: bool) -> Result<()> {
        if self.sandbox_info.is_none() {
            return Ok(());
        }
        let commands =
            crate::session::config::repo_config::resolve_before_start_hooks(&self.source_profile);
        if commands.is_empty() {
            if let Some(sb) = self.sandbox_info.as_mut() {
                sb.before_start_env.clear();
            }
            return Ok(());
        }
        let already_minted = self
            .sandbox_info
            .as_ref()
            .is_some_and(|s| !s.before_start_env.is_empty());
        if !force && already_minted {
            return Ok(());
        }

        let hook_env = crate::session::config::repo_config::lifecycle_env_vars(self);
        let project_path = PathBuf::from(&self.project_path);
        // Feed the session's sandbox env into the hook so it can read a
        // per-session value (e.g. `$TEST_VAR`) to scope what it mints.
        // Repo-contributed env is filtered out so an untrusted repo can't
        // influence the host hook's environment.
        let session_env = self
            .sandbox_info
            .as_ref()
            .map(|sb| {
                crate::session::environment::session_host_env_pairs(
                    &self.source_profile,
                    &project_path,
                    sb,
                )
            })
            .unwrap_or_default();
        let minted = crate::session::config::repo_config::run_before_start_hooks(
            &commands,
            &project_path,
            &hook_env,
            &session_env,
        )?;
        if let Some(sb) = self.sandbox_info.as_mut() {
            sb.before_start_env = minted;
        }
        Ok(())
    }

    /// Mint the `host_hooks.before_session` environment for a host
    /// (non-sandboxed) session launch.
    ///
    /// No-ops for a sandboxed session so a launch runs exactly one of the two
    /// env-minting hooks: `before_start` on container bring-up,
    /// `before_session` on host spawn. Nothing is cached, unlike
    /// [`Self::ensure_before_start_env`], which stashes its result on
    /// `SandboxInfo` so re-attaching a live container does not re-mint, a host
    /// launch always spawns a fresh agent process, so re-running the hook is
    /// both correct and the point (short-lived values get refreshed).
    ///
    /// Gated on [`Self::is_sandboxed`] rather than `sandbox_info.is_some()` so
    /// the condition matches how `build_launch_command` picks its branch: an
    /// instance carrying disabled `SandboxInfo` builds a host command, and so
    /// must mint here, or `before_session` would silently not run for it.
    ///
    /// Resolved from global + profile config only; a repo cannot contribute the
    /// command. See [`crate::session::config::repo_config::resolve_before_session_hooks`].
    pub(super) fn mint_host_session_env(&mut self) -> Result<()> {
        self.pending_host_env.clear();
        if self.is_sandboxed() {
            return Ok(());
        }
        let commands =
            crate::session::config::repo_config::resolve_before_session_hooks(&self.source_profile);
        if commands.is_empty() {
            return Ok(());
        }
        let hook_env = crate::session::config::repo_config::lifecycle_env_vars(self);
        self.pending_host_env = crate::session::config::repo_config::run_before_session_hooks(
            &commands,
            Path::new(&self.project_path),
            &hook_env,
            &[],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression for issue #2414: a sandboxed worktree session's
    /// `container_workdir()` must stay pinned to what the container was created
    /// with, even after the host worktree's git linkage breaks.
    ///
    /// When the worktree's admin entry under `<main>/.git/worktrees/<name>` is
    /// pruned, the `.git` file's gitdir no longer resolves, `compute_volume_paths`
    /// can't find the main repo, and it silently collapses to
    /// `/workspace/<basename>` -- a path the container never mounted -- so a
    /// `docker exec -w` dies with `chdir to cwd ... no such file or directory`.
    /// The create-time-pinned `SandboxInfo::container_workdir` defends against
    /// that drift.
    #[test]
    fn container_workdir_stays_pinned_when_worktree_linkage_breaks() {
        use tempfile::TempDir;
        let root = TempDir::new().unwrap();
        // An orphaned worktree: a `.git` file whose gitdir points nowhere,
        // exactly the state a pruned admin entry leaves behind.
        let worktree = root.path().join("myrepo-worktrees").join("contexec");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(
            worktree.join(".git"),
            "gitdir: ../../does-not-exist/.git/worktrees/contexec\n",
        )
        .unwrap();

        let mut inst = Instance::new("contexec", worktree.to_str().unwrap());
        inst.sandbox_info = Some(SandboxInfo {
            enabled: true,
            container_id: None,
            image: "img".to_string(),
            container_name: "aoe-sandbox-test".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
        });

        // Bug reproduction: with nothing pinned, the live recompute can't resolve
        // the orphaned worktree and falls back to the basename. This is the path
        // that produced the `chdir to cwd ("/workspace/contexec")` failure.
        assert_eq!(inst.container_workdir(), "/workspace/contexec");

        // Fix: the value the container was actually built with is returned
        // verbatim, so the exec targets a path that exists in the container.
        let pinned = "/workspace/myrepo-worktrees/contexec".to_string();
        inst.sandbox_info.as_mut().unwrap().container_workdir = Some(pinned.clone());
        assert_eq!(inst.container_workdir(), pinned);
    }
}
