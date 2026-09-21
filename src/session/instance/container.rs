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

    /// The host environment the agent process will actually see: the static profile `environment`
    /// list with every `before_session`-minted key dropped, then the minted pairs appended.
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

    /// Whether this session still reads the shared sandbox store, so its next
    /// container launch first copies that store; see [`Self::move_sandbox_store`].
    pub fn sandbox_store_move_pending(&self) -> bool {
        self.is_sandboxed()
            && self.sandbox_store_generation < container_config::CURRENT_SANDBOX_STORE_GENERATION
    }

    /// Move this session's sandbox store into the private layout, narrating the copy to `reporter`.
    pub fn move_sandbox_store(
        &self,
        reporter: Option<crate::migrations::progress::Reporter>,
    ) -> Result<bool> {
        if DockerContainer::from_session_id(&self.id).is_running()? {
            return Ok(false);
        }
        crate::migrations::migrate_sandbox_store_for_with(&self.id, reporter)?;
        Ok(true)
    }

    /// Adopt a reused container: its identity publisher counts only when the pane can reach it,
    /// and the workdir it was built with is what gets pre-trusted.
    fn finish_container_reuse(
        &mut self,
        container: &containers::DockerContainer,
        config: &crate::containers::ContainerConfig,
        detect_as: &str,
    ) -> Result<()> {
        self.identity_publisher_launched = config.identity_publisher_installed
            && identity_publisher_mount_matches(container, config)?
            && identity_publisher_dependencies_available(container)
            && self.hook_session_publisher_allowed_by_argv();
        self.backfill_container_workdir(container);
        container_config::ensure_folder_trust_config_for_active_agent(
            &self.tool,
            Some(detect_as),
            &self.source_profile,
            &self.id,
            &self.container_workdir(),
            self.is_yolo_mode(),
        );
        Ok(())
    }

    pub fn get_container_for_instance(&mut self) -> Result<containers::DockerContainer> {
        let image = self
            .sandbox_info
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Cannot ensure container for non-sandboxed session"))?
            .image
            .clone();
        let container = DockerContainer::new(&self.id, &image);
        // Charge the sandbox store move to the session that needs it, at the one chokepoint every
        // entry point shares.
        if self.sandbox_store_move_pending() {
            match self.move_sandbox_store(Some(crate::migrations::progress::tracing_reporter())) {
                Ok(true) => self.reconcile_from_disk(),
                Ok(false) => {}
                Err(error) => tracing::warn!(
                    session_id = %self.id,
                    %error,
                    "sandbox store move deferred; session continues on its shared store"
                ),
            }
        }
        self.warn_legacy_agent_config_mounts();
        let _transition_lock =
            if self.sandbox_store_generation < container_config::CURRENT_SANDBOX_STORE_GENERATION {
                Some(crate::session::acquire_storage_shared_flock(
                    &crate::session::get_app_dir()?,
                    crate::migrations::v027_isolate_sandbox_stores::LOCK,
                )?)
            } else {
                None
            };

        // A container built for another agent mounts that agent's config. Decide on the disk row
        // and a resolved profile.
        if container.exists()? {
            let reloaded = self.try_reconcile_from_disk();
            if container.agent_tool_matches(&self.container_agent_identity()?)? == Some(false) {
                reloaded.context(
                    "cannot confirm the session's tool before removing its sandbox container",
                )?;
                tracing::info!(
                    target: "containers.runtime",
                    session = %self.id,
                    "removing sandbox container built for another tool; it will be recreated"
                );
                container.remove(true)?;
            } else if let Err(error) = reloaded {
                tracing::warn!(
                    target: "session.store",
                    session = %self.id,
                    error = %format_args!("{error:#}"),
                    "failed to reload disk state before reusing the sandbox container; using in-memory value"
                );
            }
        }
        // After every reload above, which may have replaced the tool.
        let detect_as = self.effective_detect_as().into_owned();

        // Direct is_running()? / exists()?
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
            // Still rotating the copy in its store.
            if self.predates_shared_credential(&container, &detect_as)? {
                anyhow::bail!(
                    "running sandbox {} predates the shared credential file; stop it, then relaunch to rebuild it",
                    self.id
                );
            }
            // Not a come-up: the credential file stays with the containers'
            // own rotation, and is only seeded when it holds none.
            let fold = container_config::CredentialFold::SeedOnly;
            container_config::refresh_agent_configs_for_instance(
                &self.effective_profile(),
                &self.id,
                &self.tool,
                Some(detect_as.as_str()),
                fold,
            );
            let config = self.build_container_config_with(fold)?;
            self.finish_container_reuse(&container, &config, &detect_as)?;
            return Ok(container);
        }

        if self.sandbox_store_generation < container_config::CURRENT_SANDBOX_STORE_GENERATION {
            anyhow::bail!(
                "sandbox store transition is pending for {}; stop the other sandboxed sessions sharing this agent's store, then relaunch it or run `aoe migrate`",
                self.id
            );
        }

        let mut recreate = false;
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
                    container_config::CredentialFold::Freshest,
                );
                let config = self.build_container_config()?;
                // Built before its agent shared a credential file, so it mounts only the store,
                // whose copy the come-up no longer refreshes.
                recreate = container.shared_credential_mounts_match(&config)? == Some(false);
                if recreate {
                    container.remove(false)?;
                } else {
                    container_config::place_shadowed_credential_mountpoints(&config);
                    container.start()?;
                    self.finish_container_reuse(&container, &config, &detect_as)?;
                    return Ok(container);
                }
            }
        }

        // Ensure image is available (always pulls to get latest)
        let runtime = containers::get_container_runtime();
        runtime.ensure_image(&image)?;

        // Mint before building the container config so the docker-run env also carries the values
        // (leak-safe via the inherit path in run_create).
        if !recreate {
            self.ensure_before_start_env(true)?;
        }
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
        container_config::place_shadowed_credential_mountpoints(&config);
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

    fn container_agent_identity(&self) -> Result<String> {
        container_config::container_agent_identity(
            &self.tool,
            Some(&self.effective_detect_as()),
            &self.source_profile,
        )
        .context("cannot resolve the session's agent to check its sandbox container")
    }

    /// Whether the session's container was created before its agent shared a credential file, so
    /// the copy in its store is a token chain the container is still rotating.
    pub(crate) fn predates_shared_credential(
        &self,
        container: &DockerContainer,
        detect_as: &str,
    ) -> Result<bool> {
        if !container_config::agent_shares_credential_file(
            &self.effective_profile(),
            &self.tool,
            Some(detect_as),
        ) {
            return Ok(false);
        }
        Ok(container.carries_shared_credential_label()? == Some(false))
    }

    fn ensure_container_hook_mount_source(&self) {
        if let Err(error) = crate::hooks::ensure_instance_dir_path(&self.id) {
            tracing::warn!(
                target: "session.profile",
                "Failed to prepare hook directory before container launch: {error:#}"
            );
        }
    }

    /// Backfill [`SandboxInfo::container_workdir`] from a live container for a session created
    /// before that field existed (or one whose value was cleared).
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

    /// Get the container working directory for this instance. The working directory a `docker exec`
    /// into this session's sandbox must chdir to.
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

    /// Kept out of `build_container_config` so the diagnostic fires once per
    /// preparation: a launch can build the config more than once.
    fn warn_legacy_agent_config_mounts(&self) {
        let Some(home) = dirs::home_dir() else {
            return;
        };
        let Ok(config) =
            crate::session::config::profile_config::resolve_config(&self.effective_profile())
        else {
            return;
        };
        let Some(directory) = config.session.agent_config_dir_for(&self.tool, &home) else {
            return;
        };
        if let Some(entry) = config.sandbox.extra_volumes.iter().find(|entry| {
            entry
                .split_once(':')
                .is_some_and(|(source, _)| Path::new(source).starts_with(&directory))
        }) {
            tracing::warn!(
                target: "session.profile",
                agent = %self.tool,
                agent_config_dir = %directory.display(),
                extra_volume = %entry,
                "sandbox.extra_volumes includes a source in the declared agent_config_dir tree; \
                 manual agent-config mounts may bypass per-session isolation and staged folder trust. \
                 Remove manual agent-config mounts and, inside the sandbox, preserve AoE-provided config-dir variables"
            );
        }
    }

    pub(super) fn build_container_config(&self) -> Result<crate::containers::ContainerConfig> {
        self.build_container_config_with(container_config::CredentialFold::Freshest)
    }

    /// [`Self::build_container_config`] with `fold` deciding what the build
    /// may put in the credential file the agent's sandboxes share.
    fn build_container_config_with(
        &self,
        fold: container_config::CredentialFold,
    ) -> Result<crate::containers::ContainerConfig> {
        self.ensure_container_hook_mount_source();
        let detect_as = self.effective_detect_as();
        let sandbox = self
            .sandbox_info
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("sandbox_info missing for sandboxed session"))?;
        // Resolve the user-selected agent (e.g. Kiro `--agent NAME`) so the sandbox installs status
        // hooks into that agent's config, matching the host path.
        let merge_selected = crate::session::config::profile_config::resolve_config_or_warn(
            &self.effective_profile(),
        )
        .session
        .merge_hooks_into_selected_agent;
        let selected_agent = if merge_selected {
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
                .with_selected_agent(selected_agent.as_deref())
                .with_credential_fold(fold),
            self.is_yolo_mode(),
            &self.id,
            self.workspace_info.as_ref(),
            &self.source_profile,
        )
    }

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
        // Feed the session's sandbox env into the hook so it can read a per-session value (e.g.
        // `$TEST_VAR`) to scope what it mints.
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

    /// Mint the `host_hooks.before_session` environment for a host (non-sandboxed) session launch.
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
    use crate::session::instance::test_helpers::*;

    #[test]
    fn hook_mount_source_is_restored_for_agent_without_hooks() {
        let mut inst = Instance::new("agent without hooks", "/tmp/test");
        inst.id = format!("restart-hooks-{}", Uuid::new_v4().simple());
        inst.tool = "bash".to_string();

        let hook_dir = crate::hooks::ensure_instance_dir_path(&inst.id).unwrap();
        crate::hooks::cleanup_hook_status_dir(&inst.id);
        assert!(!hook_dir.exists());

        inst.ensure_container_hook_mount_source();
        assert!(hook_dir.is_dir());

        crate::hooks::cleanup_hook_status_dir(&inst.id);
    }

    /// Regression for issue #2414.
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
        inst.sandbox_info = Some(test_sandbox("aoe-sandbox-test", None));

        // Bug reproduction: with nothing pinned, the live recompute can't resolve the orphaned
        // worktree and falls back to the basename.
        assert_eq!(inst.container_workdir(), "/workspace/contexec");

        // Fix: the value the container was actually built with is returned
        // verbatim, so the exec targets a path that exists in the container.
        let pinned = "/workspace/myrepo-worktrees/contexec".to_string();
        inst.sandbox_info.as_mut().unwrap().container_workdir = Some(pinned.clone());
        assert_eq!(inst.container_workdir(), pinned);
    }

    #[test]
    #[serial_test::serial]
    #[serial_test::serial(hook_base)]
    fn legacy_agent_config_mount_warns_once_for_selected_profile_and_agent() {
        use std::fs;
        use std::sync::Mutex;

        let home = tempfile::TempDir::new().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(home.path());
        let (_hooks, _, _hook_dir) = crate::hooks::test_support::BaseGuard::ready();
        let profile = "sandbox-store-diagnostic";
        let _registry = crate::tmux::status_rules::ProfileRegistryGuard::take(profile);
        let app_dir = crate::session::get_app_dir().unwrap();
        fs::write(
            app_dir.join("config.toml"),
            r#"
[session.custom_agents]
claude-personal = "claude"
[session.agent_detect_as]
claude-personal = "claude"
[session.agent_config_dir]
claude-personal = "~/.claude-global"
"#,
        )
        .unwrap();
        let profile_path =
            crate::session::config::profile_config::get_profile_config_path(profile).unwrap();
        fs::create_dir_all(profile_path.parent().unwrap()).unwrap();
        let declared = home.path().join("account");
        let legacy = declared.join("sandbox");
        fs::create_dir_all(&legacy).unwrap();
        let legacy_json = r#"{"hasCompletedOnboarding":true}"#;
        fs::write(legacy.join(".claude.json"), legacy_json).unwrap();
        let source = declared.display();
        let cases = [
            (
                "descendants",
                "claude-personal",
                vec![
                    format!("{source}/sandbox:/root/legacy-account"),
                    format!("{source}/templates:/templates:ro"),
                ],
                1,
            ),
            (
                "root",
                "claude-personal",
                vec![format!("{source}:/account")],
                1,
            ),
            (
                "boundaries",
                "claude-personal",
                vec![
                    format!("{source}-other:/root/.claude"),
                    format!("/outside:{source}/sandbox"),
                    format!("{source}/sandbox"),
                ],
                0,
            ),
            (
                "other-agent",
                "another-agent",
                vec![format!("{source}/sandbox:/root/legacy-account")],
                0,
            ),
        ];
        let mut instance = Instance::new("diagnostic", home.path().to_str().unwrap());
        instance.tool = "claude-personal".to_string();
        instance.source_profile = profile.to_string();
        instance.sandbox_info = Some(test_sandbox("diagnostic", None));
        for (case, declared_agent, entries, expected) in cases {
            fs::write(
                &profile_path,
                format!(
                    "[session.agent_config_dir]\n{declared_agent} = \"~/account\"\n[sandbox]\nextra_volumes = {entries:?}\n"
                ),
            )
            .unwrap();
            let log_path = home.path().join(format!("{case}.log"));
            let subscriber = tracing_subscriber::fmt()
                .with_max_level(tracing::Level::WARN)
                .without_time()
                .with_ansi(false)
                .with_writer(Mutex::new(fs::File::create(&log_path).unwrap()))
                .finish();
            tracing::subscriber::with_default(subscriber, || {
                instance.warn_legacy_agent_config_mounts();
                if case == "descendants" {
                    // Preparing the config again must not duplicate the diagnostic.
                    for _ in 0..2 {
                        instance.build_container_config().unwrap();
                    }
                }
            });
            let logs = fs::read_to_string(log_path).unwrap();
            let warnings: Vec<_> = logs.lines().collect();
            assert_eq!(warnings.len(), expected, "{case}: {logs}");
            if expected == 1 {
                let warning = warnings[0];
                assert!(warning.contains("WARN") && warning.contains("session.profile"));
                assert!(warning.contains("agent=claude-personal"), "{warning}");
                assert!(
                    warning.contains(&format!("agent_config_dir={source}")),
                    "{warning}"
                );
                assert!(warning.contains(&entries[0]), "{warning}");
            }
        }
        assert_eq!(
            fs::read_to_string(legacy.join(".claude.json")).unwrap(),
            legacy_json
        );
    }

    /// A container built for another tool is recreated rather than reused (#3976).
    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn container_built_for_another_tool_is_removed_before_reuse() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let bin = temp.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let calls_path = temp.path().join("runtime-calls");
        let label_path = temp.path().join("tool-label");
        let removed_path = temp.path().join("removed");
        // A stopped container carrying the tool label read from `label_path`. Removal makes it
        // absent.
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{calls}'\n\
             if [ \"$1\" = rm ]; then touch '{removed}'; exit 0; fi\n\
             if [ \"$1\" = container ] && [ \"$2\" = inspect ]; then\n\
             if [ -e '{removed}' ]; then echo 'Error: No such container: c' >&2; exit 1; fi\n\
             case \"$*\" in\n\
             *agent-tool*) cat '{label}' ;;\n\
             *sandbox-store-generation*) echo 2 ;;\n\
             *State.Running*) echo false ;;\n\
             esac\n\
             exit 0\n\
             fi\n\
             echo 'permission denied' >&2\nexit 1\n",
            calls = calls_path.display(),
            removed = removed_path.display(),
            label = label_path.display(),
        );
        for binary in ["docker", "podman", "container"] {
            let path = bin.join(binary);
            std::fs::write(&path, &script).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let _path = crate::session::test_support::path_prepended(&bin);
        let profile = "agent-tool-label";
        let _registry = crate::tmux::status_rules::ProfileRegistryGuard::take(profile);
        let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
        // `alias-c` is aliased only in config, never on the session row.
        std::fs::write(
            crate::session::get_app_dir().unwrap().join("config.toml"),
            "[session.agent_detect_as]\nalias-c = \"claude\"\n",
        )
        .unwrap();
        let profile_config =
            crate::session::config::profile_config::get_profile_config_path(profile).unwrap();

        enum Disk {
            Absent,
            Corrupt,
            /// A peer persisted this `(tool, detect_as)` after the in-memory copy.
            Row(&'static str, &'static str),
            /// A persisted row whose profile config cannot be parsed.
            RowBrokenProfile(&'static str, &'static str),
        }
        // `store` is the agent config root the reuse path refreshed, if any.
        let cases = [
            (("codex", ""), "claude", Disk::Absent, 1, None),
            (
                ("codex", ""),
                "claude",
                Disk::Row("claude", ""),
                0,
                Some(".claude"),
            ),
            (("codex", ""), "codex", Disk::Absent, 0, Some(".codex")),
            (("codex", ""), "", Disk::Absent, 0, Some(".codex")),
            (("codex", ""), "claude", Disk::Corrupt, 0, None),
            (("codex", ""), "codex", Disk::Corrupt, 0, Some(".codex")),
            (("claude", ""), "claude", Disk::Row("codex", ""), 1, None),
            (
                ("alias-a", "claude"),
                "alias-b:codex",
                Disk::Row("alias-b", "codex"),
                0,
                Some(".codex"),
            ),
            (("alias-a", "codex"), "alias-a", Disk::Absent, 1, None),
            (
                ("alias-c", ""),
                "alias-c:claude",
                Disk::Row("alias-c", ""),
                0,
                Some(".claude"),
            ),
            (
                ("alias-c", ""),
                "alias-c:claude",
                Disk::RowBrokenProfile("alias-c", ""),
                0,
                None,
            ),
        ];
        for ((tool, detect_as), built_for, disk, expected_removals, expected_store) in cases {
            let _ = std::fs::remove_file(&calls_path);
            let _ = std::fs::remove_file(&removed_path);
            std::fs::write(&label_path, built_for).unwrap();
            let mut instance = Instance::new("tool label", temp.path().to_str().unwrap());
            instance.tool = tool.to_string();
            instance.detect_as = detect_as.to_string();
            instance.source_profile = profile.to_string();
            instance.sandbox_info = Some(test_sandbox("tool-label", None));
            let _ = std::fs::remove_file(storage.sessions_path());
            let _ = std::fs::remove_file(&profile_config);
            storage
                .update(|instances, _groups| {
                    instances.clear();
                    if let Disk::Row(tool, detect_as) | Disk::RowBrokenProfile(tool, detect_as) =
                        disk
                    {
                        let mut row = instance.clone();
                        row.tool = tool.to_string();
                        row.detect_as = detect_as.to_string();
                        instances.push(row);
                    }
                    Ok(())
                })
                .unwrap();
            match disk {
                Disk::Corrupt => std::fs::write(storage.sessions_path(), "not json").unwrap(),
                Disk::RowBrokenProfile(..) => {
                    std::fs::create_dir_all(profile_config.parent().unwrap()).unwrap();
                    std::fs::write(&profile_config, "not toml [").unwrap();
                    // A failed resolve elsewhere leaves the alias registry empty.
                    crate::session::config::profile_config::resolve_config_or_warn(profile);
                }
                Disk::Absent | Disk::Row(..) => {}
            }
            let container = DockerContainer::from_session_id(&instance.id).name;

            let error = instance
                .get_container_for_instance()
                .err()
                .expect("the fake runtime fails every launch");

            let calls = std::fs::read_to_string(&calls_path).unwrap_or_default();
            let removals = calls
                .lines()
                .filter(|line| line.starts_with("rm -f") && line.ends_with(&container))
                .count();
            let case = format!("{tool}/{detect_as} built_for={built_for:?}: {error:#}\n{calls}");
            assert_eq!(removals, expected_removals, "{case}");
            let stores: Vec<_> = [".claude", ".codex"]
                .into_iter()
                .filter(|root| {
                    temp.path()
                        .join(root)
                        .join("sandbox-v2")
                        .join(&instance.id)
                        .exists()
                })
                .collect();
            assert_eq!(stores, Vec::from_iter(expected_store), "{case}");
            if let Disk::RowBrokenProfile(..) = disk {
                assert!(
                    format!("{error:#}").contains("cannot resolve the session's agent"),
                    "{case}"
                );
            }
        }
        let _ = std::fs::remove_file(&profile_config);

        // The label written at create is the identity the check compares.
        for (tool, detect_as) in [("codex", ""), ("alias-b", "codex")] {
            let mut instance = Instance::new("tool label", temp.path().to_str().unwrap());
            instance.tool = tool.to_string();
            instance.detect_as = detect_as.to_string();
            instance.source_profile = profile.to_string();
            instance.sandbox_info = Some(test_sandbox("tool-label", None));
            assert_eq!(
                instance.build_container_config().unwrap().agent_tool,
                instance.container_agent_identity().unwrap(),
                "{tool}/{detect_as}"
            );
        }
    }
}
