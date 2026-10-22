//! Container-backed sessions: config, workdir, and the environment minted
//! before start.

use super::*;
use crate::session::sandbox_login::LaunchAuth;

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

/// How long a resolved `host_hooks.before_start` environment stays good for the next
/// container come-up. A session held longer re-runs the hook, so it never starts on a stale
/// minted token.
pub(crate) const BEFORE_START_RESOLUTION_MAX_AGE: std::time::Duration =
    std::time::Duration::from_secs(300);

/// The `host_hooks.before_start` environment resolved for a session's next come-up. Secret:
/// kept in memory only, never stored, logged or sent anywhere. Copies of it (the row's cache,
/// the gate's environment, the container's run environment) are short-lived values the
/// process drops with their owners; only the slot's copy is overwritten on removal.
struct ResolvedBeforeStart {
    env: Vec<(String, String)>,
    at: std::time::Instant,
    /// Of the configured hook commands, so a changed hook never reuses an older env.
    fingerprint: u64,
}

impl ResolvedBeforeStart {
    /// Fresh for the hook with `fingerprint`, with at least `margin` of its life left.
    fn fresh_for(&self, fingerprint: u64, margin: std::time::Duration) -> bool {
        self.fingerprint == fingerprint
            && self.at.elapsed() + margin < BEFORE_START_RESOLUTION_MAX_AGE
    }
}

/// A session's resolution state. `forgotten` is the tombstone of a slot whose session was
/// deleted while a resolver held it: whoever finds it set drops its result and starts over
/// on the session's next slot.
#[derive(Default)]
struct Slot {
    forgotten: std::sync::atomic::AtomicBool,
    state: std::sync::Mutex<Option<ResolvedBeforeStart>>,
}

type ResolutionSlot = std::sync::Arc<Slot>;

/// One slot per session id, shared by every copy of the row and every caller in the process:
/// the resolution is the session's, not a copy's, so concurrent pre-checks and the come-up
/// run the hook once between them. Holding a slot's lock is the resolution guard.
static BEFORE_START_SLOTS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, ResolutionSlot>>,
> = std::sync::LazyLock::new(Default::default);

fn resolution_slot(session_id: &str) -> ResolutionSlot {
    let mut slots = BEFORE_START_SLOTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    std::sync::Arc::clone(slots.entry(session_id.to_string()).or_default())
}

/// Drops (and overwrites) a resolution found stale: expired, or of a hook that has changed.
fn drop_if_stale(state: &mut Option<ResolvedBeforeStart>, fingerprint: u64) {
    if state
        .as_ref()
        .is_some_and(|resolved| !resolved.fresh_for(fingerprint, std::time::Duration::ZERO))
    {
        if let Some(mut stale) = state.take() {
            scrub(&mut stale.env);
        }
    }
}

/// What the sandbox login admission of a launch ran with, kept on the row for the come-up
/// of that launch. The request's environment may hold a key: it is never printed.
#[derive(Clone)]
pub(crate) struct LaunchAdmission {
    agent: Option<String>,
    request_env: Vec<(String, String)>,
}

impl std::fmt::Debug for LaunchAdmission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LaunchAdmission")
            .field("agent", &self.agent)
            .field(
                "request_env",
                &format_args!("<{} redacted>", self.request_env.len()),
            )
            .finish()
    }
}

fn hook_fingerprint(commands: &[String]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    commands.hash(&mut hasher);
    hasher.finish()
}

/// How long one resolution of the `host_hooks.before_start` hooks (all of its commands
/// together) may take when the caller set no hook deadline of its own (a bounded restart or
/// recovery installs one). The run is cut off and its process tree killed at the deadline,
/// so a hook that never exits cannot hold the session's resolution slot, a blocking thread
/// or an HTTP request for ever. A process that daemonizes or reparents itself escapes the
/// kill.
const BEFORE_START_HOOK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// How long a restart wants a resolution to stay fresh: it re-checks strictly under the
/// locks, and the admission that follows its destructive steps must not find the resolution
/// expired.
const RESTART_RESOLUTION_MARGIN: std::time::Duration = std::time::Duration::from_secs(60);

/// How long past the hook's own deadline a caller waits for the session's slot, held by
/// another caller's hook run, before it gives up with [`BeforeStartHookError::Busy`].
#[cfg(not(test))]
const SLOT_WAIT_MARGIN: std::time::Duration = std::time::Duration::from_secs(5);
#[cfg(test)]
const SLOT_WAIT_MARGIN: std::time::Duration = std::time::Duration::from_millis(100);

fn before_start_hook_timeout() -> std::time::Duration {
    crate::session::recovery::current_hook_timeout().unwrap_or(BEFORE_START_HOOK_TIMEOUT)
}

/// Why the `host_hooks.before_start` hook produced no environment for a launch, in words that
/// are safe to show a client: never the command, the hook's output or its environment (which
/// may carry tokens). The server logs only the class and the exit status or timeout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum BeforeStartHookError {
    #[error("the before_start hook failed")]
    Failed,
    #[error("the before_start hook timed out")]
    TimedOut,
    #[error("the before_start hook could not be started")]
    NotStarted,
    /// Another caller's run of the hook for this session did not finish in time. A hook
    /// that calls back into the daemon about its own session during admission ends here.
    #[error("the before_start hook is still running for this session")]
    Busy,
}

/// Takes the slot's lock, waiting at most the hook deadline plus a margin for a run in
/// progress. A timed-out wait leaves the slot as it is.
fn lock_slot(
    slot: &Slot,
) -> Result<std::sync::MutexGuard<'_, Option<ResolvedBeforeStart>>, BeforeStartHookError> {
    let deadline = std::time::Instant::now() + before_start_hook_timeout() + SLOT_WAIT_MARGIN;
    loop {
        match slot.state.try_lock() {
            Ok(guard) => return Ok(guard),
            Err(std::sync::TryLockError::Poisoned(poisoned)) => return Ok(poisoned.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => {}
        }
        if std::time::Instant::now() >= deadline {
            tracing::warn!(
                target: "session.profile",
                "timed out waiting for another before_start hook run of this session"
            );
            return Err(BeforeStartHookError::Busy);
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Overwrites the values in place before they are dropped (best effort: copies made
/// elsewhere are not reachable from here).
fn scrub(env: &mut [(String, String)]) {
    for (key, value) in env {
        for text in [key, value] {
            let len = text.len();
            text.replace_range(.., &"\0".repeat(len));
        }
    }
}

/// Forgets a session's resolved hook environment (the session was deleted, trashed or
/// archived, or has no hook configured any more), so a secret nothing will consume does not
/// stay in memory. A resolver that holds the slot meanwhile finds its tombstone and drops
/// what it resolved instead of storing it.
pub(crate) fn forget_before_start_resolution(session_id: &str) {
    let slot = BEFORE_START_SLOTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(session_id);
    if let Some(slot) = slot {
        slot.forgotten
            .store(true, std::sync::atomic::Ordering::SeqCst);
        if let Ok(mut state) = slot.state.try_lock() {
            if let Some(mut resolved) = state.take() {
                scrub(&mut resolved.env);
            }
        }
    }
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
        self.resolved_host_environment_from(self.profile_host_environment())
    }

    pub(super) fn resolved_host_environment_from(
        &self,
        profile_environment: Vec<String>,
    ) -> Vec<String> {
        let mut environment = crate::session::environment::drop_shadowed_host_entries(
            profile_environment,
            &self.pending_host_env,
        );
        environment.extend(self.pending_host_env.iter().map(|(key, value)| {
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
            && !crate::migrations::v033_isolate_sandbox_content::instance_ready(self)
                .unwrap_or(false)
    }

    /// Move this session's sandbox store into the private layout, narrating
    /// the copy to `reporter`. `Ok(false)` means the container is up, so the
    /// store cannot move yet and nothing was attempted. The row is read back
    /// from disk by the caller, not here: the TUI's copy lives in its own
    /// mirror.
    ///
    /// A running container is worth skipping outright: its cohort cannot
    /// move while it is up, so the pass is guaranteed to refuse, and it is
    /// not free, since planning takes the v027 lock and every registry's
    /// storage lock, on which `Storage::update` waits.
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

    pub fn get_container_for_instance(&mut self) -> Result<containers::DockerContainer> {
        self.get_container_until_cancelled(&tokio_util::sync::CancellationToken::new())
    }

    /// [`Self::get_container_for_instance`] that stops before each costly step, and
    /// kills an image pull, once `cancel` fires.
    pub fn get_container_until_cancelled(
        &mut self,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<containers::DockerContainer> {
        self.ensure_container(cancel, false)
    }

    /// [`Self::get_container_for_instance`] for a persisted session: the stored row is reread
    /// under the lifecycle flock, held through the runtime start or create, so a completed archive
    /// or trash cannot be followed by a container come-up. Hooks run before the flock is taken.
    pub(super) fn get_container_admitted(&mut self) -> Result<containers::DockerContainer> {
        self.ensure_container(&tokio_util::sync::CancellationToken::new(), true)
    }

    fn ensure_container(
        &mut self,
        cancel: &tokio_util::sync::CancellationToken,
        admit: bool,
    ) -> Result<containers::DockerContainer> {
        // Taken inside the shared transition flock below; no holder of the lifecycle flock waits
        // for that flock exclusively, so the order cannot cycle.
        let admission = |instance: &Self| admit.then(|| instance.lock_for_input()).transpose();
        let checkpoint = || {
            if cancel.is_cancelled() {
                anyhow::bail!("sandbox start cancelled");
            }
            Ok(())
        };
        let image = self
            .sandbox_info
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Cannot ensure container for non-sandboxed session"))?
            .image
            .clone();
        let container = DockerContainer::new(&self.id, &image);
        // A come-up resolves its `before_start` environment, and gates on it again if it has
        // to mint, before any destructive step below (store move, container removal, image
        // pull): a refusal then leaves everything as it was. A running container comes up
        // nothing and leaves the session's resolution alone.
        self.before_start_minted_for_launch = false;
        if self.launch_admission.is_some()
            && !self.before_start_commands().is_empty()
            && !container.is_running()?
        {
            self.ensure_before_start_env(true)?;
            self.before_start_minted_for_launch = true;
        }
        // Charge the sandbox store move to the session that needs it, at the
        // one chokepoint every entry point shares: tmux launches, ACP
        // structured sessions and a bare container terminal all arrive here.
        // The TUI runs it ahead of time on a worker so this is a no-op there;
        // see `tui::store_move_poller`. It must stay above the shared flock
        // below. Failure is not permission to launch on unproven native state.
        if self.sandbox_store_move_pending() {
            if !self.move_sandbox_store(Some(crate::migrations::progress::tracing_reporter()))? {
                anyhow::bail!(
                    "sandbox {} must be stopped before native history can be isolated",
                    self.id
                );
            }
            self.reconcile_from_disk();
        }
        self.warn_legacy_agent_config_mounts();

        // A container built for another agent mounts that agent's config.
        // Decide on the disk row and a resolved profile: a stale in-memory copy
        // can match an old container or mismatch one a peer rebuilt, and a
        // defaulted config would misread a valid one. A failed reload still
        // permits reuse, never removal.
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
        let command = self.get_tool_command().to_owned();
        // Admit the reconciled tool before refreshing or starting its native store.
        let _transition_lock =
            crate::migrations::v033_isolate_sandbox_content::admit_fresh_instance(self)?;

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
            // Refresh would fold a rotating private copy into the shared credential.
            if self.predates_shared_credential(&container, &command)? {
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
                Some(command.as_str()),
                fold,
                std::path::Path::new(&self.container_workdir()),
            );
            let config = self.build_container_config_with(fold)?;
            self.finish_container_reuse(&container, &config, &command)?;
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
                    Some(command.as_str()),
                    container_config::CredentialFold::Freshest,
                    std::path::Path::new(&self.container_workdir()),
                );
                let config = self.build_container_config()?;
                // Built before its agent shared a credential file, so it
                // mounts only the store, whose copy the come-up no longer
                // refreshes.
                recreate = container.shared_credential_mounts_match(&config)? == Some(false);
                if recreate {
                    container.remove(false)?;
                } else {
                    let _admission = admission(self)?;
                    container_config::place_shadowed_credential_mountpoints(&config);
                    container.start()?;
                    self.finish_container_reuse(&container, &config, &command)?;
                    return Ok(container);
                }
            }
        }

        // Ensure image is available (always pulls to get latest)
        checkpoint()?;
        let runtime = containers::get_container_runtime();
        runtime.ensure_image(&image, cancel)?;
        checkpoint()?;

        // Mint before building the container config so the docker-run env also
        // carries the values (leak-safe via the inherit path in run_create).
        // A container just removed for its credential mount was minted above.
        if !recreate {
            self.ensure_before_start_env(true)?;
        }
        let _admission = admission(self)?;
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
        checkpoint()?;
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

    fn finish_container_reuse(
        &mut self,
        container: &containers::DockerContainer,
        config: &crate::containers::ContainerConfig,
        command: &str,
    ) -> Result<()> {
        self.identity_publisher_launched = config.identity_publisher_installed
            && identity_publisher_mount_matches(container, config)?
            && identity_publisher_dependencies_available(container)
            && self.hook_session_publisher_allowed_by_argv();
        if let Some(sandbox) = self.sandbox_info.as_mut() {
            if sandbox.container_id.is_none() {
                sandbox.container_id = Some(container.id()?);
            }
        }
        self.backfill_container_workdir(container);
        container_config::ensure_folder_trust_config_for_active_agent(
            &self.tool,
            Some(command),
            &self.source_profile,
            &self.id,
            &self.container_workdir(),
            self.is_yolo_mode(),
        );
        Ok(())
    }

    fn container_agent_identity(&self) -> Result<String> {
        container_config::container_agent_identity(
            &self.tool,
            Some(self.get_tool_command()),
            &self.source_profile,
        )
        .context("cannot resolve the session's agent to check its sandbox container")
    }

    /// Whether the session's container was created before its agent shared a
    /// credential file, so the copy in its store is a token chain the
    /// container is still rotating. Read from the create-time label rather
    /// than from the config, since building the config folds that copy in.
    pub(crate) fn predates_shared_credential(
        &self,
        container: &DockerContainer,
        command: &str,
    ) -> Result<bool> {
        if !container_config::agent_shares_credential_file(
            &self.effective_profile(),
            &self.tool,
            Some(command),
        ) {
            return Ok(false);
        }
        Ok(container.carries_shared_credential_label()? == Some(false))
    }

    /// Refuses a sandbox launch in `separate_login` mode while the shared credential its
    /// agent needs holds none, before any container or agent starts. The agent's login
    /// integration says whether this launch uses that credential at all: another auth
    /// route, such as a cloud provider pick or an API key, does not. An unreadable file
    /// counts as holding none. Keyed on `tool`, as the container's agent config mounts are,
    /// so another agent run in a `tool = "claude"` session is gated too.
    ///
    /// This is the hold decision only: a launch the active runtime cannot log in at all
    /// ([`SandboxLoginUnsupported`]) is never held, see [`Self::ensure_sandbox_launchable_for`].
    pub(crate) fn ensure_sandbox_login(&self) -> Result<(), SandboxLoginRequired> {
        self.ensure_sandbox_login_for(None, &[])
    }

    /// [`Self::ensure_sandbox_login`] for a launch that carries a request's `provider_env`,
    /// which the launch forwards into the sandbox and so counts as its auth route, and the
    /// agent the request selected. `launch_agent` is the agent the launch will run; its
    /// adapter's allowlist decides which host variables cross, so it is what admission
    /// reads, and `None` is the stored agent. Both are read for this decision only, never
    /// stored; a launch without the request (a respawn, the reconciler) is admitted on what
    /// is stored.
    pub(crate) fn ensure_sandbox_login_for(
        &self,
        launch_agent: Option<&str>,
        request_env: &[(String, String)],
    ) -> Result<(), SandboxLoginRequired> {
        match self.sandbox_login_gate(launch_agent, request_env) {
            Some((container_config::LoginGate::LoginRequired, target)) => {
                Err(self.login_required(&target))
            }
            _ => Ok(()),
        }
    }

    /// The whole admission of a sandbox launch: the hold decision of
    /// [`Self::ensure_sandbox_login_for`], and the refusal of a launch that needs a login the
    /// active runtime cannot perform ([`SandboxLoginUnsupported`]). The error downcasts to
    /// one of the two, or is a [`BeforeStartHookError`] for the hook the decision had to run.
    ///
    /// A launch the cheap decision refuses may still carry its auth route in the environment
    /// that hook produces. See [`Self::resolve_before_start_if_refused`]: only then the hook
    /// runs, and the decision is taken again on the real environment. What the admission ran
    /// with (target agent, request environment) stays on this row for the come-up of the same
    /// launch, which gates on it again whenever it has to mint.
    pub(crate) fn ensure_sandbox_launchable_for(
        &mut self,
        launch_agent: Option<&str>,
        request_env: &[(String, String)],
    ) -> anyhow::Result<()> {
        self.launch_admission = Some(LaunchAdmission {
            agent: launch_agent.map(str::to_string),
            request_env: request_env.to_vec(),
        });
        let gate = self.resolve_before_start_if_refused(
            launch_agent,
            request_env,
            std::time::Duration::ZERO,
        )?;
        self.admission_result(gate)
    }

    /// The gate decision for this launch, resolving the `host_hooks.before_start`
    /// environment first when the cheap decision refuses and a configured hook has not run
    /// for this launch yet.
    ///
    /// The cheap decision counts only a hook environment that is resolved and still has
    /// `margin` of its life left (the session's slot, below): a cache left by an earlier
    /// come-up proves nothing, because the hook may emit something else now. Resolution runs
    /// the hook, one deadline for all its commands, through the mint the container come-up
    /// uses, under the session's resolution guard, so concurrent callers (sends, a switch, a
    /// launch, on any copy of the row) and the come-up that follows run it once between
    /// them; whoever comes second finds the first one's result. The decision is then taken on
    /// the environment just resolved, not on a re-read of the slot. The row is reconciled
    /// first, so the hook sees the row the launch will run. An empty result is a result: a
    /// refused attempt does not run the hook again until the come-up consumes the
    /// resolution, it expires ([`BEFORE_START_RESOLUTION_MAX_AGE`]) or the configured hook
    /// changes. A hook failure is a [`BeforeStartHookError`] and resolves nothing.
    pub(crate) fn resolve_before_start_if_refused(
        &mut self,
        launch_agent: Option<&str>,
        request_env: &[(String, String)],
        margin: std::time::Duration,
    ) -> anyhow::Result<Option<(container_config::LoginGate, container_config::LoginTarget)>> {
        let refused = |gate: &Option<(container_config::LoginGate, _)>| {
            matches!(
                gate,
                Some((
                    container_config::LoginGate::LoginRequired
                        | container_config::LoginGate::Unsupported,
                    _
                ))
            )
        };
        let mut gate = self.sandbox_login_gate_with(
            launch_agent,
            request_env,
            self.trusted_before_start_env(margin),
        );
        if refused(&gate) && self.before_start_hook_pending(margin) {
            // The hook's environment is built from the reconciled row, and the reconciled
            // row may decide differently before any hook is needed.
            if let Err(error) = self.try_reconcile_from_disk() {
                tracing::warn!(
                    target: "session.store",
                    session = %self.id,
                    error = %format_args!("{error:#}"),
                    "failed to reload disk state before resolving the before_start hook; using in-memory value"
                );
            }
            gate = self.sandbox_login_gate_with(
                launch_agent,
                request_env,
                self.trusted_before_start_env(margin),
            );
            if refused(&gate) && self.before_start_hook_pending(margin) {
                let resolved = self.resolve_before_start_in_slot(margin)?;
                gate = self.sandbox_login_gate_with(launch_agent, request_env, resolved);
            }
        }
        Ok(gate)
    }

    /// The admission's decision on what the session holds right now, running no hook and
    /// deferring nothing: a launch the cheap decision refuses is refused, whether its hook
    /// was never resolved, its resolution expired, would expire within a restart's safety
    /// margin, or a concurrent come-up consumed it. A restart re-checks with this under the
    /// lifecycle locks, immediately before it stops or discards anything (hooks may call
    /// `aoe` on the session, so none runs there).
    pub(crate) fn ensure_sandbox_launchable_strict(&self) -> anyhow::Result<()> {
        let gate = self.sandbox_login_gate_with(
            None,
            &[],
            self.trusted_before_start_env(RESTART_RESOLUTION_MARGIN),
        );
        self.admission_result(gate)
    }

    /// The restart's early resolution, before any lock is taken: like the admission, but a
    /// resolution that would expire within the restart's safety margin counts as unresolved.
    pub(crate) fn resolve_before_start_for_restart(&mut self) -> anyhow::Result<()> {
        self.resolve_before_start_if_refused(None, &[], RESTART_RESOLUTION_MARGIN)
            .map(|_| ())
    }

    /// The refusal a gate decision stands for, as the admission reports it.
    fn admission_result(
        &self,
        gate: Option<(container_config::LoginGate, container_config::LoginTarget)>,
    ) -> anyhow::Result<()> {
        match gate {
            Some((container_config::LoginGate::LoginRequired, target)) => {
                Err(self.login_required(&target).into())
            }
            Some((container_config::LoginGate::Unsupported, _)) => {
                Err(SandboxLoginUnsupported.into())
            }
            _ => Ok(()),
        }
    }

    /// The configured `host_hooks.before_start` commands of this session's profile.
    fn before_start_commands(&self) -> Vec<String> {
        crate::session::config::repo_config::resolve_before_start_hooks(&self.source_profile)
    }

    /// The hook environment the gate may count as an auth route: the session's resolved
    /// resolution with at least `margin` of life left, or nothing. A cache an earlier come-up
    /// left on the row is not trusted, and a stale resolution found here is dropped. Never
    /// blocks: a resolution in flight counts as none yet.
    fn trusted_before_start_env(&self, margin: std::time::Duration) -> Vec<(String, String)> {
        let commands = self.before_start_commands();
        if commands.is_empty() {
            return Vec::new();
        }
        let fingerprint = hook_fingerprint(&commands);
        let slot = resolution_slot(&self.id);
        let mut state = match slot.state.try_lock() {
            Ok(state) => state,
            Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return Vec::new(),
        };
        drop_if_stale(&mut state, fingerprint);
        state
            .as_ref()
            .filter(|resolved| resolved.fresh_for(fingerprint, margin))
            .map(|resolved| resolved.env.clone())
            .unwrap_or_default()
    }

    /// Whether a configured `host_hooks.before_start` hook has not run for this launch yet
    /// (or its resolution has less than `margin` left), so its environment could still
    /// change a refusal.
    fn before_start_hook_pending(&self, margin: std::time::Duration) -> bool {
        let commands = self.before_start_commands();
        if self.sandbox_info.is_none() || commands.is_empty() {
            return false;
        }
        let fingerprint = hook_fingerprint(&commands);
        let slot = resolution_slot(&self.id);
        let mut state = match slot.state.try_lock() {
            Ok(state) => state,
            Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            // In flight: resolving waits for it and finds its result.
            Err(std::sync::TryLockError::WouldBlock) => return true,
        };
        drop_if_stale(&mut state, fingerprint);
        !state
            .as_ref()
            .is_some_and(|resolved| resolved.fresh_for(fingerprint, margin))
    }

    /// Runs `body` on the session's slot with its lock held, which is the resolution guard.
    /// The wait is bounded; a slot whose session was deleted meanwhile (its tombstone is set)
    /// is left for the session's next one.
    fn with_session_slot<R>(
        &mut self,
        body: impl FnOnce(&mut Self, &mut Option<ResolvedBeforeStart>, &Slot) -> Result<R>,
    ) -> Result<R> {
        let mut body = Some(body);
        for _ in 0..3 {
            let slot = resolution_slot(&self.id);
            let mut state = lock_slot(&slot)?;
            if slot.forgotten.load(std::sync::atomic::Ordering::SeqCst) {
                continue;
            }
            if let Some(body) = body.take() {
                return body(self, &mut state, &slot);
            }
        }
        Err(BeforeStartHookError::Busy.into())
    }

    /// Resolves the session's `before_start` environment into its slot and returns it,
    /// holding the slot's lock across the check and the hook run: a caller that finds the
    /// slot resolved by another path takes that result and runs nothing.
    ///
    /// The whole resolution is bounded by one deadline ([`BEFORE_START_HOOK_TIMEOUT`], or
    /// the caller's own) and killed at it, and a caller waits for the lock only as long as
    /// that bound plus a margin, so neither a hook that never exits nor one that calls back
    /// into the daemon about this very session (the call needs this admission, which waits
    /// on this lock) holds the session: the wait times out as
    /// [`BeforeStartHookError::Busy`], the run as `TimedOut`, and the slot stays unresolved
    /// for a later retry. A `before_start` hook must not call back into the daemon about its
    /// own session during admission.
    fn resolve_before_start_in_slot(
        &mut self,
        margin: std::time::Duration,
    ) -> Result<Vec<(String, String)>> {
        let commands = self.before_start_commands();
        if commands.is_empty() {
            forget_before_start_resolution(&self.id);
            return Ok(Vec::new());
        }
        let fingerprint = hook_fingerprint(&commands);
        self.with_session_slot(|this, state, slot| {
            drop_if_stale(state, fingerprint);
            if let Some(resolved) = state
                .as_ref()
                .filter(|resolved| resolved.fresh_for(fingerprint, margin))
            {
                let env = resolved.env.clone();
                if let Some(sandbox) = this.sandbox_info.as_mut() {
                    sandbox.before_start_env.clone_from(&env);
                }
                return Ok(env);
            }
            this.mint_before_start_env()?;
            let env = this
                .sandbox_info
                .as_ref()
                .map(|sandbox| sandbox.before_start_env.clone())
                .unwrap_or_default();
            // The session was deleted while the hook ran: its result is not kept.
            if !slot.forgotten.load(std::sync::atomic::Ordering::SeqCst) {
                if let Some(mut replaced) = state.take() {
                    scrub(&mut replaced.env);
                }
                *state = Some(ResolvedBeforeStart {
                    env: env.clone(),
                    at: std::time::Instant::now(),
                    fingerprint,
                });
            }
            Ok(env)
        })
    }

    /// Ages the session's resolution past its limit, for a test of the expiry.
    #[cfg(test)]
    fn expire_before_start_resolution(&self) {
        let slot = resolution_slot(&self.id);
        let mut state = slot
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(resolved) = state.as_mut() {
            resolved.at = std::time::Instant::now()
                .checked_sub(BEFORE_START_RESOLUTION_MAX_AGE * 2)
                .expect("an instant that far back");
        }
    }

    /// Leaves the session's resolution only `remaining` of its life, for a test of a
    /// restart's safety margin.
    #[cfg(test)]
    fn age_before_start_resolution(&self, remaining: std::time::Duration) {
        let slot = resolution_slot(&self.id);
        let mut state = slot
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(resolved) = state.as_mut() {
            resolved.at = std::time::Instant::now()
                .checked_sub(BEFORE_START_RESOLUTION_MAX_AGE - remaining)
                .expect("an instant that far back");
        }
    }

    /// Whether the session's slot still holds a resolution, for a test.
    #[cfg(test)]
    fn holds_before_start_resolution(&self) -> bool {
        resolution_slot(&self.id)
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
    }

    /// The refusal for a launch gated on `target`. The recovery it prints must release this
    /// launch: it runs under the session's profile (`-p`, unless that is the default one the
    /// CLI uses anyway), and names the session's own command (`--command`) only when
    /// `--tool` alone reaches another file than `target` and the command is what leads to
    /// it.
    fn login_required(&self, target: &container_config::LoginTarget) -> SandboxLoginRequired {
        let profile = self.effective_profile();
        let session_command = Some(self.command.as_str()).filter(|c| !c.is_empty());
        let reaches = |command: Option<&str>| {
            container_config::login_target_with_command(&profile, &self.tool, command)
                .is_some_and(|t| t.credential == target.credential)
        };
        let command = session_command.filter(|_| !reaches(None) && reaches(session_command));
        SandboxLoginRequired {
            tool: self.tool.clone(),
            credential: target.credential.clone(),
            profile: (profile != crate::session::config::effective_profile(""))
                .then(|| profile.clone()),
            command: command.map(str::to_string),
        }
    }

    /// [`Self::ensure_sandbox_launchable_for`] for a launch without a request.
    pub(crate) fn ensure_sandbox_launchable(&mut self) -> anyhow::Result<()> {
        self.ensure_sandbox_launchable_for(None, &[])
    }

    /// The gate decision for this launch with the target it is gated on; `None` when the
    /// session is not sandboxed or its agent has no sandbox login.
    fn sandbox_login_gate(
        &self,
        launch_agent: Option<&str>,
        request_env: &[(String, String)],
    ) -> Option<(container_config::LoginGate, container_config::LoginTarget)> {
        self.sandbox_login_gate_with(
            launch_agent,
            request_env,
            self.trusted_before_start_env(std::time::Duration::ZERO),
        )
    }

    /// [`Self::sandbox_login_gate`] on an explicit `before_start` environment: the one a
    /// come-up has just minted, which is not resolved in any slot.
    fn sandbox_login_gate_with(
        &self,
        launch_agent: Option<&str>,
        request_env: &[(String, String)],
        hook_env: Vec<(String, String)>,
    ) -> Option<(container_config::LoginGate, container_config::LoginTarget)> {
        if !self.is_sandboxed() {
            return None;
        }
        let profile = self.effective_profile();
        let mode = crate::session::config::profile_config::resolve_config_or_warn(&profile)
            .sandbox
            .shared_credential;
        // The same resolution `aoe sandbox login` runs, with this session's command, so the
        // file the gate waits for is the one the login writes. The agent identity comes from
        // the profile's settings alone, as the container mount resolves the agent it mounts
        // (`build_container_config`): an agent only the repository's settings declare gets no
        // shared credential mount, so gating on its file would hold the session for a login
        // the sandbox never reads. Only the env route below follows the repository too,
        // because the launch reads profile and repository for its adapter's allowlist.
        let target =
            container_config::login_target_with_command(&profile, &self.tool, Some(&self.command))?;
        let gate = container_config::login_gate(
            mode,
            containers::get_container_runtime().kind,
            target.login,
            &self.launch_auth(&profile, launch_agent, request_env, hook_env),
            || {
                container_config::read_shared_credential(&target.credential).unwrap_or_else(|e| {
                tracing::warn!(target: "session.profile", session = %self.id, "Failed to read the shared credential: {e:#}");
                None
            })
            },
        );
        Some((gate, target))
    }

    /// The auth route this launch runs with: the sandbox environment for the session's own
    /// provider pick, which a container built for an earlier pick does not reflect yet.
    fn launch_auth(
        &self,
        profile: &str,
        launch_agent: Option<&str>,
        request_env: &[(String, String)],
        hook_env: Vec<(String, String)>,
    ) -> LaunchAuth {
        let Some(sandbox) = self.sandbox_info.as_ref() else {
            return LaunchAuth::default();
        };
        // Only a resolved, fresh hook environment counts as an auth route: a cache an earlier
        // come-up left on the row says nothing about what the hook emits now.
        let sandbox = SandboxInfo {
            provider: self.agent_provider.clone(),
            before_start_env: hook_env,
            ..sandbox.clone()
        };
        let config = crate::session::environment::resolved_sandbox_config(
            profile,
            std::path::Path::new(&self.project_path),
        );
        let mut env = crate::session::environment::collect_environment(&config, &sandbox);
        // A structured launch also forwards the request's `provider_env` and the adapter's
        // allowlisted host variables (an API key or token the daemon has set), after the
        // sandbox environment has claimed its own; the launch's own resolution decides which
        // of them cross, so this sees what the launch will run with.
        if self.view == View::Structured {
            let agent = launch_agent
                .or(self.agent_name.as_deref())
                .unwrap_or(&self.tool);
            // The launch resolves profile and repository settings, so admission does too.
            let session = crate::session::config::repo_config::resolve_config_with_repo_or_warn(
                profile,
                std::path::Path::new(&self.project_path),
            )
            .session;
            env = crate::acp::sandbox::effective_launch_env(agent, &session, &env, request_env);
        }
        LaunchAuth::from_env(&env)
    }

    fn ensure_container_hook_mount_source(&self) {
        if let Err(error) = crate::hooks::ensure_instance_dir_path(&self.id) {
            tracing::warn!(
                target: "session.profile",
                "Failed to prepare hook directory before container launch: {error:#}"
            );
        }
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
    /// `/workspace/<basename>` (a path the container never mounted), and the
    /// exec dies with `chdir to cwd ("/workspace/<name>") ... no such file or
    /// directory`. A recorded `WorkspaceInfo` is a third source: it replays the
    /// repos the workspace was created with instead of rediscovering the git
    /// linkage, so it cannot collapse that way either. The live computation
    /// survives only as a fallback for a session with neither a pin nor a
    /// recorded workspace, where there is nothing to replay.
    pub fn container_workdir(&self) -> String {
        container_config::container_workdir_for(
            &self.project_path,
            self.sandbox_info
                .as_ref()
                .and_then(|info| info.container_workdir.as_deref()),
            self.workspace_info.as_ref(),
        )
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
            container_config::ContainerAgentSelection::new(
                &self.tool,
                Some(self.get_tool_command()),
            )
            .with_selected_agent(selected_agent.as_deref())
            .with_credential_fold(fold),
            container_config::SandboxYolo {
                enabled: self.is_yolo_mode(),
            },
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
    /// `force` is a container come-up (create or restart). Without it the container is
    /// already running, nothing comes up, and the session's slot is left alone: values are
    /// backfilled only if the row has none (from a fresh resolution, copied not consumed, or
    /// a mint).
    ///
    /// A come-up takes the resolution the sandbox login admission left in the session's slot
    /// ([`Self::resolve_before_start_if_refused`]) and consumes it, so the hook runs once per
    /// come-up whichever copy of the row resolved it. When it has to mint itself (the slot
    /// was empty, expired, of another hook, or consumed by a concurrent come-up) and an
    /// admission ran on this row, the gate is decided again on the new environment, with the
    /// same target agent and request environment, before anything is created: the launch is
    /// refused cleanly if no auth route remains. The mint runs under the slot lock, so no
    /// resolver runs the hook in parallel, and leaves the slot empty. A hook failure is
    /// propagated so the container does not come up without the values the agent depends on.
    /// Hooks are resolved from profile/global config only, never from the repo.
    pub(super) fn ensure_before_start_env(&mut self, force: bool) -> Result<()> {
        if self.sandbox_info.is_none() {
            return Ok(());
        }
        let commands = self.before_start_commands();
        if commands.is_empty() {
            forget_before_start_resolution(&self.id);
            return self.mint_before_start_env();
        }
        if !force {
            return self.backfill_before_start_env(&commands);
        }
        // Already resolved for this launch, ahead of its destructive steps.
        if std::mem::take(&mut self.before_start_minted_for_launch) {
            return Ok(());
        }
        self.come_up_before_start_env(&commands)
    }

    /// The values of an already-running container's row, when it has none: a fresh
    /// resolution is copied (the slot is not consumed, no container comes up), else the
    /// hooks run once.
    fn backfill_before_start_env(&mut self, commands: &[String]) -> Result<()> {
        if self
            .sandbox_info
            .as_ref()
            .is_some_and(|s| !s.before_start_env.is_empty())
        {
            return Ok(());
        }
        let fingerprint = hook_fingerprint(commands);
        self.with_session_slot(|this, state, _| {
            drop_if_stale(state, fingerprint);
            if let Some(resolved) = state
                .as_ref()
                .filter(|r| r.fresh_for(fingerprint, std::time::Duration::ZERO))
            {
                if let Some(sandbox) = this.sandbox_info.as_mut() {
                    sandbox.before_start_env.clone_from(&resolved.env);
                }
                return Ok(());
            }
            this.mint_before_start_env()
        })
    }

    /// The come-up's values: the consumed resolution, or a mint gated again.
    fn come_up_before_start_env(&mut self, commands: &[String]) -> Result<()> {
        let fingerprint = hook_fingerprint(commands);
        self.with_session_slot(|this, state, _| {
            match state.take() {
                Some(mut resolved)
                    if resolved.fresh_for(fingerprint, std::time::Duration::ZERO) =>
                {
                    if let Some(sandbox) = this.sandbox_info.as_mut() {
                        sandbox.before_start_env.clone_from(&resolved.env);
                    }
                    scrub(&mut resolved.env);
                    return Ok(());
                }
                Some(mut stale) => scrub(&mut stale.env),
                None => {}
            }
            this.mint_before_start_env()?;
            if let Some(admission) = this.launch_admission.clone() {
                let minted = this
                    .sandbox_info
                    .as_ref()
                    .map(|sandbox| sandbox.before_start_env.clone())
                    .unwrap_or_default();
                let gate = this.sandbox_login_gate_with(
                    admission.agent.as_deref(),
                    &admission.request_env,
                    minted,
                );
                this.admission_result(gate)?;
            }
            Ok(())
        })
    }

    /// Runs the configured `before_start` hooks now, all commands under one deadline
    /// ([`BEFORE_START_HOOK_TIMEOUT`] or the caller's own, the time left for each command),
    /// and caches their environment (clearing it when none is configured). The one mint
    /// behind both the container come-up and the admission's resolution; a failure leaves
    /// the cache as it was and is a [`BeforeStartHookError`]: its words never carry the
    /// command or the hook's output.
    fn mint_before_start_env(&mut self) -> Result<()> {
        if self.sandbox_info.is_none() {
            return Ok(());
        }
        let commands = self.before_start_commands();
        if commands.is_empty() {
            if let Some(sb) = self.sandbox_info.as_mut() {
                sb.before_start_env.clear();
            }
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
        let deadline = std::time::Instant::now() + before_start_hook_timeout();
        let mut minted: Vec<(String, String)> = Vec::new();
        for command in &commands {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Err(self
                    .log_before_start_failure(BeforeStartHookError::TimedOut, None)
                    .into());
            }
            let _bounded = crate::session::HookTimeoutScope::new(remaining);
            let pairs = crate::session::config::repo_config::run_before_start_hooks(
                std::slice::from_ref(command),
                &project_path,
                &hook_env,
                &session_env,
            )
            .map_err(|error| self.classify_before_start_failure(&error))?;
            for (key, value) in pairs {
                minted.retain(|(existing, _)| existing != &key);
                minted.push((key, value));
            }
        }
        if let Some(sb) = self.sandbox_info.as_mut() {
            sb.before_start_env = minted;
        }
        Ok(())
    }

    /// The client-safe class of a failed hook run; the server logs it with the exit status or
    /// the timeout, never the command or the hook's output.
    fn classify_before_start_failure(&self, error: &anyhow::Error) -> BeforeStartHookError {
        use crate::session::config::repo_config::{HookExit, HookTimeout};
        let timeout = error.chain().find_map(|c| c.downcast_ref::<HookTimeout>());
        let exit = error.chain().find_map(|c| c.downcast_ref::<HookExit>());
        let class = if timeout.is_some() {
            BeforeStartHookError::TimedOut
        } else if exit.is_some() {
            BeforeStartHookError::Failed
        } else {
            BeforeStartHookError::NotStarted
        };
        self.log_before_start_failure(class, exit.and_then(|e| e.code))
    }

    fn log_before_start_failure(
        &self,
        class: BeforeStartHookError,
        exit_status: Option<i32>,
    ) -> BeforeStartHookError {
        tracing::warn!(
            target: "session.profile",
            session = %self.id,
            class = ?class,
            exit_status = ?exit_status,
            "host before_start hook produced no environment"
        );
        class
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
    use crate::session::config::SharedCredentialMode;

    fn sandboxed_instance(tool: &str) -> Instance {
        let mut inst = Instance::new("login gate", "/tmp/aoe-login-gate");
        inst.tool = tool.to_string();
        inst.sandbox_info = Some(SandboxInfo {
            provider: None,
            enabled: true,
            container_id: None,
            image: "test:latest".to_string(),
            container_name: "aoe-login-gate".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
        });
        inst
    }

    fn set_shared_credential_mode(mode: SharedCredentialMode) {
        crate::session::config::update_config(|c| c.sandbox.shared_credential = mode).unwrap();
    }

    /// Writes `content` to the shared Claude credential under `home`.
    fn write_shared_credential(home: &std::path::Path, content: &str) {
        let root = home.join(".claude").join("sandbox-v2");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(".credentials.json"), content).unwrap();
    }

    fn usable_credential() -> String {
        let expires_at = chrono::Utc::now().timestamp_millis() + 3_600_000;
        crate::session::sandbox_login::claude::fixtures::usable(expires_at as u64)
    }

    #[test]
    #[serial_test::serial]
    #[serial_test::serial(hook_base)]
    fn sandbox_login_gate_instance_requires_login_in_separate_login() {
        let (_hg, _, _base) = crate::hooks::test_support::BaseGuard::ready();
        let home = tempfile::TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_home(home.path());
        set_shared_credential_mode(SharedCredentialMode::SeparateLogin);
        let claude = sandboxed_instance("claude");

        assert_eq!(
            claude.ensure_sandbox_login(),
            Err(crate::session::test_support::login_required_under(
                "claude",
                home.path()
            )),
            "no file"
        );
        write_shared_credential(home.path(), "{}");
        assert_eq!(
            claude.ensure_sandbox_login(),
            Err(crate::session::test_support::login_required_under(
                "claude",
                home.path()
            ))
        );
        write_shared_credential(home.path(), &usable_credential());
        assert_eq!(claude.ensure_sandbox_login(), Ok(()));

        #[cfg(unix)]
        if !nix::unistd::geteuid().is_root() {
            use std::os::unix::fs::PermissionsExt;
            let shared = home.path().join(".claude/sandbox-v2/.credentials.json");
            std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o000)).unwrap();
            let unreadable = claude.ensure_sandbox_login();
            std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o600)).unwrap();
            assert_eq!(
                unreadable,
                Err(crate::session::test_support::login_required_under(
                    "claude",
                    home.path()
                )),
                "unreadable file"
            );
        }

        write_shared_credential(home.path(), "{}");
        let mut host = sandboxed_instance("claude");
        host.sandbox_info = None;
        assert_eq!(host.ensure_sandbox_login(), Ok(()));
    }

    #[test]
    #[serial_test::serial]
    #[serial_test::serial(hook_base)]
    fn sandbox_login_gate_host_seed_and_codex_launch_unchanged() {
        let (_hg, _, _base) = crate::hooks::test_support::BaseGuard::ready();
        let home = tempfile::TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_home(home.path());
        write_shared_credential(home.path(), "{}");

        set_shared_credential_mode(SharedCredentialMode::HostSeed);
        assert_eq!(sandboxed_instance("claude").ensure_sandbox_login(), Ok(()));
        set_shared_credential_mode(SharedCredentialMode::SeparateLogin);
        assert_eq!(sandboxed_instance("codex").ensure_sandbox_login(), Ok(()));
    }

    /// A launch on another auth route (a cloud provider pick, an API key or token in the
    /// sandbox environment) never reads the shared OAuth login, so an empty file does not
    /// hold it; an OAuth-dependent launch with the same file stays held.
    #[test]
    #[serial_test::serial]
    #[serial_test::serial(hook_base)]
    fn sandbox_login_gate_holds_only_launches_that_use_the_shared_login() {
        let (_hg, _, _base) = crate::hooks::test_support::BaseGuard::ready();
        let home = tempfile::TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_home(home.path());
        let _env = crate::session::test_support::EnvGuard::unset(&[
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
        ]);
        set_shared_credential_mode(SharedCredentialMode::SeparateLogin);
        write_shared_credential(home.path(), "{}");

        // (provider pick, sandbox environment, held)
        let cases: [(Option<&str>, &[&str], bool); 8] = [
            (None, &[], true),
            (Some("api"), &[], true),
            (Some("bedrock"), &[], false),
            (Some("vertex"), &[], false),
            (None, &["ANTHROPIC_API_KEY=sk-test"], false),
            (Some("api"), &["CLAUDE_CODE_OAUTH_TOKEN=token"], false),
            (None, &["ANTHROPIC_AUTH_TOKEN=token"], false),
            (None, &["UNRELATED=1"], true),
        ];
        for (provider, env, held) in cases {
            let mut inst = sandboxed_instance("claude");
            inst.agent_provider = provider.map(str::to_string);
            if let Some(sandbox) = inst.sandbox_info.as_mut() {
                sandbox.extra_env = Some(env.iter().map(|e| e.to_string()).collect());
            }
            assert_eq!(
                inst.ensure_sandbox_login().is_err(),
                held,
                "{provider:?} {env:?}"
            );
        }

        // The pick on the session decides, not the provider its container was built for.
        let mut stale = sandboxed_instance("claude");
        stale.agent_provider = Some("bedrock".to_string());
        if let Some(sandbox) = stale.sandbox_info.as_mut() {
            sandbox.provider = Some("api".to_string());
        }
        assert_eq!(stale.ensure_sandbox_login(), Ok(()));
    }

    /// A structured launch also gets the adapter's allowlisted host variables, so a daemon
    /// host exporting an API key or setup token is not held; a terminal session gets none of
    /// them, so the same host still holds it. An empty value is no route.
    #[test]
    #[serial_test::serial]
    #[serial_test::serial(hook_base)]
    fn sandbox_login_gate_counts_the_host_variables_a_structured_launch_forwards() {
        let (_hg, _, _base) = crate::hooks::test_support::BaseGuard::ready();
        let home = tempfile::TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_home(home.path());
        let _unset = crate::session::test_support::EnvGuard::unset(&[
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "ANTHROPIC_AUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN",
        ]);
        set_shared_credential_mode(SharedCredentialMode::SeparateLogin);
        write_shared_credential(home.path(), "{}");
        let structured = {
            let mut inst = sandboxed_instance("claude");
            inst.view = View::Structured;
            inst
        };
        let terminal = sandboxed_instance("claude");

        let unset = crate::session::test_support::EnvGuard::unset(&["ANTHROPIC_API_KEY"]);
        assert!(
            structured.ensure_sandbox_login().is_err(),
            "no host key: held"
        );
        drop(unset);

        let _empty = crate::session::test_support::EnvGuard::set(&[("ANTHROPIC_API_KEY", "")]);
        assert!(
            structured.ensure_sandbox_login().is_err(),
            "an empty key is no route"
        );
        drop(_empty);

        let _key = crate::session::test_support::EnvGuard::set(&[("ANTHROPIC_API_KEY", "sk-host")]);
        assert_eq!(
            structured.ensure_sandbox_login(),
            Ok(()),
            "forwarded host key"
        );
        assert!(
            terminal.ensure_sandbox_login().is_err(),
            "a terminal session forwards no host variables"
        );
        // The sandbox environment still claims a key first: an explicit provider pick that
        // blanks nothing relevant leaves the host key in effect, and the pick of API stays held
        // only without any key.
        let mut picked = structured.clone();
        picked.agent_provider = Some("api".to_string());
        assert_eq!(picked.ensure_sandbox_login(), Ok(()));
    }

    /// A structured launch forwards the request's `provider_env` between the sandbox
    /// environment and the daemon's host variables, first claim winning even when empty;
    /// admission resolves the same order, so a request-only key lifts the hold and an empty
    /// claim by a higher source beats a key from a lower one.
    #[test]
    #[serial_test::serial]
    #[serial_test::serial(hook_base)]
    fn sandbox_login_gate_counts_the_request_key_in_the_launch_precedence() {
        let (_hg, _, _base) = crate::hooks::test_support::BaseGuard::ready();
        let home = tempfile::TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_home(home.path());
        let _unset = crate::session::test_support::EnvGuard::unset(&[
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "ANTHROPIC_AUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "ANTHROPIC_API_KEY",
        ]);
        set_shared_credential_mode(SharedCredentialMode::SeparateLogin);
        write_shared_credential(home.path(), "{}");
        let structured = |sandbox_env: &[&str]| {
            let mut inst = sandboxed_instance("claude");
            inst.view = View::Structured;
            if let Some(sandbox) = inst.sandbox_info.as_mut() {
                sandbox.extra_env = Some(sandbox_env.iter().map(|e| e.to_string()).collect());
            }
            inst
        };
        let request = |value: &str| vec![("ANTHROPIC_API_KEY".to_string(), value.to_string())];
        let held = |inst: &Instance, request_env: &[(String, String)]| {
            inst.ensure_sandbox_login_for(None, request_env).is_err()
        };

        let plain = structured(&[]);
        assert!(!held(&plain, &request("sk-request")), "request-only key");
        assert!(held(&plain, &[]), "no key: the OAuth-only refusal");
        assert!(
            held(&plain, &request("")),
            "an empty request key is no route"
        );
        assert!(
            held(&plain, &[("UNRELATED".to_string(), "1".to_string())]),
            "an unrelated request variable is no route"
        );
        assert!(
            held(
                &plain,
                &[("CLAUDE_CONFIG_DIR".to_string(), "/x".to_string())]
            ),
            "a denied request key claims nothing"
        );
        assert!(
            sandboxed_instance("claude")
                .ensure_sandbox_login_for(None, &request("sk-request"))
                .is_err(),
            "a terminal launch forwards no request"
        );
        assert!(held(&plain, &[]), "nothing was remembered from the request");

        // The sandbox environment claims first, even with an empty value.
        assert!(
            !held(&structured(&["ANTHROPIC_API_KEY=sk-sandbox"]), &request("")),
            "sandbox key beats an empty request key"
        );
        assert!(
            held(&structured(&["ANTHROPIC_API_KEY="]), &request("sk-request")),
            "an empty sandbox key beats the request key"
        );

        // The request claims ahead of the daemon's host variables.
        let _host =
            crate::session::test_support::EnvGuard::set(&[("ANTHROPIC_API_KEY", "sk-host")]);
        assert!(!held(&plain, &[]), "host key alone");
        assert!(
            held(&plain, &request("")),
            "an empty request key beats the host key"
        );
        assert!(
            !held(&plain, &request("sk-request")),
            "request key beats the host key"
        );
        assert!(
            held(&structured(&["ANTHROPIC_API_KEY="]), &[]),
            "an empty sandbox key beats the host key"
        );
    }

    #[test]
    fn sandbox_login_gate_survives_anyhow_context() {
        let error = anyhow::Error::new(crate::session::test_support::login_required("claude"))
            .context("ensuring sandbox container");
        assert_eq!(
            error.downcast_ref::<SandboxLoginRequired>(),
            Some(&crate::session::test_support::login_required("claude"))
        );
    }

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

    /// The agent an explicit spawn selects decides which adapter allowlist admission reads,
    /// as the launch it admits forwards host variables by that adapter: the stored agent
    /// stands only when the request names none, and nothing of the request is stored.
    #[test]
    #[serial_test::serial]
    #[serial_test::serial(hook_base)]
    fn sandbox_login_gate_reads_the_selected_launch_agents_allowlist() {
        let (_hg, _, _base) = crate::hooks::test_support::BaseGuard::ready();
        let home = tempfile::TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_home(home.path());
        let _unset = crate::session::test_support::EnvGuard::unset(&[
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "ANTHROPIC_AUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN",
        ]);
        let _host =
            crate::session::test_support::EnvGuard::set(&[("ANTHROPIC_API_KEY", "sk-host")]);
        set_shared_credential_mode(SharedCredentialMode::SeparateLogin);
        write_shared_credential(home.path(), "{}");
        let stored = |agent: &str| {
            let mut inst = sandboxed_instance("claude");
            inst.view = View::Structured;
            inst.agent_name = Some(agent.to_string());
            inst
        };
        let held = |inst: &Instance, selected: Option<&str>| {
            inst.ensure_sandbox_login_for(selected, &[]).is_err()
        };

        // The stored agent's allowlist does not forward the key; the selected one does.
        let codex = stored("codex");
        assert!(held(&codex, None), "stored codex forwards no host key");
        assert!(
            !held(&codex, Some("claude")),
            "the selected claude adapter forwards the key"
        );
        // The reverse: the stored agent would forward it, the selected one does not.
        let claude = stored("claude");
        assert!(!held(&claude, None), "stored claude forwards the host key");
        assert!(
            held(&claude, Some("codex")),
            "the selected codex adapter forwards no host key"
        );
        assert!(!held(&claude, Some("claude")), "same agent, same decision");
        // The request's own key still counts with either agent.
        let request = [("ANTHROPIC_API_KEY".to_string(), "sk-request".to_string())];
        assert!(codex.ensure_sandbox_login_for(None, &request).is_ok());

        // Nothing of the request reached the session or the shared credential.
        assert_eq!(codex.agent_name.as_deref(), Some("codex"));
        assert_eq!(claude.agent_name.as_deref(), Some("claude"));
        assert_eq!(
            std::fs::read_to_string(home.path().join(".claude/sandbox-v2/.credentials.json"))
                .unwrap(),
            "{}"
        );
        assert!(held(&codex, None), "the selection was not remembered");
    }

    /// The adapter an agent name resolves to may come from the repository's own settings:
    /// execution resolves profile and repository, so admission must too. A key the daemon
    /// holds crosses for a repository-only `agent_detect_as`, and a launch without one is
    /// still held.
    #[test]
    #[serial_test::serial]
    #[serial_test::serial(hook_base)]
    fn sandbox_login_gate_reads_the_adapter_a_repository_setting_declares() {
        let (_hg, _, _base) = crate::hooks::test_support::BaseGuard::ready();
        let home = tempfile::TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_home(home.path());
        let _unset = crate::session::test_support::EnvGuard::unset(&[
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "ANTHROPIC_AUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "ANTHROPIC_API_KEY",
        ]);
        set_shared_credential_mode(SharedCredentialMode::SeparateLogin);
        write_shared_credential(home.path(), "{}");
        let project = tempfile::TempDir::new().unwrap();
        let repo_config = project.path().join(".agent-of-empires");
        std::fs::create_dir_all(&repo_config).unwrap();
        std::fs::write(
            repo_config.join("config.toml"),
            "[session.agent_detect_as]\npersonal = \"claude\"\n",
        )
        .unwrap();
        let mut inst = sandboxed_instance("claude");
        inst.project_path = project.path().to_string_lossy().into_owned();
        inst.view = View::Structured;
        inst.agent_name = Some("personal".to_string());

        assert!(
            inst.ensure_sandbox_login().is_err(),
            "no allowlisted key: held"
        );
        let _key = crate::session::test_support::EnvGuard::set(&[("ANTHROPIC_API_KEY", "sk-host")]);
        assert_eq!(
            inst.ensure_sandbox_login(),
            Ok(()),
            "the repository's adapter forwards the host key, as the launch does"
        );
        assert!(
            inst.ensure_sandbox_launchable().is_ok(),
            "the whole admission agrees"
        );
    }

    /// Writes a repository config declaring `personal` as a Claude adapter and returns the
    /// repository directory.
    fn repository_declaring_personal_as_claude() -> tempfile::TempDir {
        let project = tempfile::TempDir::new().unwrap();
        let repo_config = project.path().join(".agent-of-empires");
        std::fs::create_dir_all(&repo_config).unwrap();
        std::fs::write(
            repo_config.join("config.toml"),
            "[session.agent_detect_as]\npersonal = \"claude\"\n",
        )
        .unwrap();
        project
    }

    /// Writes `body` as the config of profile `name`.
    fn write_profile_config(name: &str, body: &str) {
        let path = crate::session::config::profile_config::get_profile_config_path(name).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    /// The gate resolves under the session's profile, the CLI under the default one unless
    /// `-p` says otherwise: a session in another profile gets the recovery with `-p`, whose
    /// file is the gate's, while the plain form would log in the default profile's file.
    #[test]
    #[serial_test::serial]
    #[serial_test::serial(hook_base)]
    fn sandbox_login_required_names_the_profile_the_session_runs_under() {
        use crate::session::config::container_config::login_target_with_command;
        let (_hg, _, _base) = crate::hooks::test_support::BaseGuard::ready();
        let home = tempfile::TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_home(home.path());
        set_shared_credential_mode(SharedCredentialMode::SeparateLogin);
        crate::session::config::update_config(|c| {
            c.session
                .custom_agents
                .insert("personal".to_string(), "claude".to_string());
        })
        .unwrap();
        let default = crate::session::config::effective_profile("");
        let _registry = crate::tmux::status_rules::ProfileRegistryGuard::take("work");
        write_profile_config(
            "work",
            "[session.agent_config_dir]\npersonal = \"~/.claude-work\"\n",
        );
        let work_file = home
            .path()
            .join(".claude-work/sandbox-v2/.credentials.json");
        let mut inst = sandboxed_instance("personal");
        inst.source_profile = "work".to_string();

        let held = inst.ensure_sandbox_login().unwrap_err();
        assert_eq!(
            held.to_string(),
            "sandbox login required for personal: run `aoe sandbox login -p work --tool personal`"
        );
        assert_eq!(
            login_target_with_command("work", "personal", None).map(|t| t.credential),
            Some(work_file.clone()),
            "-p work resolves the gate's file"
        );
        assert_ne!(
            login_target_with_command(&default, "personal", None).map(|t| t.credential),
            Some(work_file),
            "the plain form would log in another file"
        );

        // The default profile needs no `-p`.
        let held = sandboxed_instance("personal")
            .ensure_sandbox_login()
            .unwrap_err();
        assert_eq!(
            held.to_string(),
            "sandbox login required for personal: run `aoe sandbox login --tool personal`"
        );
    }

    /// Both flags needed: the session's profile is the only one declaring the tool's
    /// directory and the session's command override is what makes the tool a login agent.
    #[test]
    #[serial_test::serial]
    #[serial_test::serial(hook_base)]
    fn sandbox_login_required_combines_profile_and_command() {
        let (_hg, _, _base) = crate::hooks::test_support::BaseGuard::ready();
        let home = tempfile::TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_home(home.path());
        set_shared_credential_mode(SharedCredentialMode::SeparateLogin);
        crate::session::config::update_config(|c| {
            c.session
                .custom_agents
                .insert("personal".to_string(), "codex".to_string());
        })
        .unwrap();
        // Resolve the default profile before `work` exists, or `work` would become it.
        let _default = crate::session::config::effective_profile("");
        let _registry = crate::tmux::status_rules::ProfileRegistryGuard::take("work");
        write_profile_config(
            "work",
            "[session.agent_config_dir]\npersonal = \"~/.claude-work\"\n",
        );
        let mut inst = sandboxed_instance("personal");
        inst.source_profile = "work".to_string();
        inst.command = "claude".to_string();

        assert_eq!(
            inst.ensure_sandbox_login().unwrap_err().to_string(),
            "sandbox login required for personal: run `aoe sandbox login -p work --tool personal --command claude`"
        );
    }

    /// Identity follows the container mount, which resolves the agent from the profile
    /// alone: a tool only the repository maps to a login agent gets no shared credential
    /// mount, so nothing is gated on one.
    #[test]
    #[serial_test::serial]
    #[serial_test::serial(hook_base)]
    fn sandbox_login_gate_ignores_an_agent_only_the_repository_declares() {
        let (_hg, _, _base) = crate::hooks::test_support::BaseGuard::ready();
        let home = tempfile::TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_home(home.path());
        set_shared_credential_mode(SharedCredentialMode::SeparateLogin);
        write_shared_credential(home.path(), "{}");
        let project = repository_declaring_personal_as_claude();
        let mut inst = sandboxed_instance("personal");
        inst.project_path = project.path().to_string_lossy().into_owned();

        assert_eq!(inst.ensure_sandbox_login(), Ok(()));
        assert!(inst.ensure_sandbox_launchable().is_ok());
    }

    #[test]
    fn sandbox_login_required_prints_profile_and_command_quoted_in_one_command() {
        let both = SandboxLoginRequired {
            tool: "personal".to_string(),
            credential: "/x/.credentials.json".into(),
            profile: Some("my work".to_string()),
            command: Some("claude --model fast".to_string()),
        };
        assert_eq!(
            shell_words::split(&both.login_command()).unwrap(),
            [
                "aoe",
                "sandbox",
                "login",
                "-p",
                "my work",
                "--tool",
                "personal",
                "--command",
                "claude --model fast",
            ]
        );
    }

    /// A session without a command of its own runs what the profile gives its tool, which is
    /// what `aoe sandbox login --tool` resolves: the gate must hold it and wait for the same
    /// file, not stop at the built-in binary or `bash`.
    #[test]
    #[serial_test::serial]
    #[serial_test::serial(hook_base)]
    fn sandbox_login_gate_resolves_the_profile_command_of_a_session_without_one() {
        use crate::session::config::container_config::login_target;
        let (_hg, _, _base) = crate::hooks::test_support::BaseGuard::ready();
        let home = tempfile::TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_home(home.path());
        set_shared_credential_mode(SharedCredentialMode::SeparateLogin);
        crate::session::config::update_config(|c| {
            c.session
                .custom_agents
                .insert("personal".to_string(), "claude".to_string());
            c.session
                .agent_config_dir
                .insert("personal".to_string(), "~/.claude-personal".to_string());
        })
        .unwrap();
        let profile = crate::session::config::effective_profile("");
        let mut inst = sandboxed_instance("personal");
        assert!(inst.command.is_empty());

        let held = inst.ensure_sandbox_login().unwrap_err();
        let login = login_target(&profile, "personal").unwrap().credential;
        assert_eq!(
            login,
            home.path()
                .join(".claude-personal/sandbox-v2/.credentials.json")
        );
        assert_eq!(
            held.credential, login,
            "the gate waits for the login's file"
        );
        assert_eq!(held.command, None, "plain --tool reaches it");

        // A command of its own still wins over the profile's.
        inst.command = "codex".to_string();
        assert_eq!(inst.ensure_sandbox_login(), Ok(()), "codex has no login");
    }

    /// The recovery the refusal prints must release the launch: the plain `--tool` form when
    /// that reaches the gate's file, the `--command` form (quoted) only when the session's own
    /// command is what leads there.
    #[test]
    #[serial_test::serial]
    #[serial_test::serial(hook_base)]
    fn sandbox_login_required_names_the_command_only_where_tool_alone_misses_the_file() {
        let (_hg, _, _base) = crate::hooks::test_support::BaseGuard::ready();
        let home = tempfile::TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_home(home.path());
        set_shared_credential_mode(SharedCredentialMode::SeparateLogin);
        crate::session::config::update_config(|c| {
            c.session
                .custom_agents
                .insert("personal".to_string(), "codex".to_string());
            c.session
                .agent_config_dir
                .insert("personal".to_string(), "~/.codex-personal".to_string());
        })
        .unwrap();
        let refused = |tool: &str, command: &str| {
            let mut inst = sandboxed_instance(tool);
            inst.command = command.to_string();
            inst.ensure_sandbox_login().unwrap_err()
        };

        let overridden = refused("personal", "claude");
        assert_eq!(overridden.command.as_deref(), Some("claude"));
        assert_eq!(
            overridden.to_string(),
            "sandbox login required for personal: run `aoe sandbox login --tool personal --command claude`"
        );

        let spaced = refused("personal", "claude --model fast");
        assert_eq!(spaced.command.as_deref(), Some("claude --model fast"));
        assert!(
            spaced
                .to_string()
                .contains("--tool personal --command 'claude --model fast'`"),
            "{spaced}"
        );
        let quoted = spaced.login_command();
        let words = shell_words::split(&quoted).unwrap();
        assert_eq!(
            words.last().map(String::as_str),
            Some("claude --model fast"),
            "the command is one shell word"
        );

        for (tool, command) in [("claude", "claude"), ("claude", "")] {
            let plain = refused(tool, command);
            assert_eq!(plain.command, None, "{tool} {command:?}");
            assert_eq!(
                plain.to_string(),
                "sandbox login required for claude: run `aoe sandbox login --tool claude`"
            );
        }
        let body = crate::server::api::sandbox_login_required_body(&overridden);
        assert_eq!(body["command"], "claude");
        assert_eq!(
            crate::server::api::sandbox_login_required_body(&refused("claude", "claude"))
                .get("command"),
            None
        );
    }

    /// Points `host_hooks.before_start` at a hook that appends a line to the returned counter
    /// file on every run, then runs `tail`. Returns the counter's path.
    fn counting_before_start_hook(dir: &std::path::Path, tail: &str) -> std::path::PathBuf {
        let counter = dir.join("hook-runs");
        let hook = format!(
            "echo run >> {}; {tail}",
            crate::session::environment::shell_escape(&counter.to_string_lossy()),
        );
        crate::session::config::update_config(|global| {
            global.host_hooks.before_start = vec![hook];
        })
        .unwrap();
        counter
    }

    fn hook_runs(counter: &std::path::Path) -> usize {
        std::fs::read_to_string(counter)
            .map(|runs| runs.lines().count())
            .unwrap_or(0)
    }

    /// Admission looks at what the launch really carries, and a trusted `before_start` host
    /// hook is part of that: its environment is not cached yet when the gate runs (always
    /// after a daemon restart). The gate resolves it lazily, only when it would refuse,
    /// through the mint the container come-up uses, so the hook runs once per come-up, a
    /// refused attempt does not run it again (an empty result is a result), and merely
    /// configuring one never admits a launch whose environment carries no auth route.
    #[test]
    #[serial_test::serial]
    #[serial_test::serial(hook_base)]
    fn sandbox_login_gate_resolves_a_before_start_hook_lazily_once_per_come_up() {
        let (_hg, _, _base) = crate::hooks::test_support::BaseGuard::ready();
        let home = tempfile::TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_home(home.path());
        let _unset = crate::session::test_support::EnvGuard::unset(&[
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "ANTHROPIC_AUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "ANTHROPIC_API_KEY",
        ]);
        set_shared_credential_mode(SharedCredentialMode::SeparateLogin);
        write_shared_credential(home.path(), "{}");
        let project = home.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let session = |view: View| {
            let mut inst = sandboxed_instance("claude");
            inst.project_path = project.to_string_lossy().into_owned();
            inst.view = view;
            inst
        };
        let required = |result: &anyhow::Result<()>| {
            result
                .as_ref()
                .err()
                .and_then(|e| e.downcast_ref::<SandboxLoginRequired>())
                .is_some()
        };

        for view in [View::Structured, View::Terminal] {
            let label = format!("{view:?}");
            let launch = |inst: &mut Instance| match view {
                View::Structured => inst.ensure_sandbox_launchable_for(None, &[]),
                View::Terminal => inst.ensure_sandbox_launchable(),
            };

            // A hook emitting the key: admitted; a second pre-check and the come-up reuse
            // its values; only the next come-up mints again.
            let counter = counting_before_start_hook(home.path(), "echo ANTHROPIC_API_KEY=sk-hook");
            let _ = std::fs::remove_file(&counter);
            let mut inst = session(view);
            assert!(
                inst.sandbox_info
                    .as_ref()
                    .unwrap()
                    .before_start_env
                    .is_empty(),
                "{label}: nothing cached at admission"
            );
            let result = launch(&mut inst);
            assert!(result.is_ok(), "{label}: {result:?}");
            assert_eq!(hook_runs(&counter), 1, "{label}: one run at admission");
            assert_eq!(
                inst.sandbox_info.as_ref().unwrap().before_start_env,
                vec![("ANTHROPIC_API_KEY".to_string(), "sk-hook".to_string())],
                "{label}: the values stay in the cache"
            );
            assert!(launch(&mut inst).is_ok(), "{label}: second pre-check");
            assert_eq!(hook_runs(&counter), 1, "{label}: it reuses the resolution");
            inst.ensure_before_start_env(true).unwrap();
            assert_eq!(hook_runs(&counter), 1, "{label}: the come-up consumes it");
            inst.ensure_before_start_env(true).unwrap();
            assert_eq!(hook_runs(&counter), 2, "{label}: the next come-up mints");

            // A restart empties the cache: it resolves again, once.
            inst.sandbox_info.as_mut().unwrap().before_start_env.clear();
            assert!(launch(&mut inst).is_ok(), "{label}");
            assert_eq!(hook_runs(&counter), 3, "{label}: resolved after a restart");

            // A hook without an auth route does not admit the launch, and a repeated refused
            // attempt does not run it again, whether it printed nothing useful or nothing.
            for (tail, what) in [("echo FOO=bar", "no auth"), ("true", "empty")] {
                let counter = counting_before_start_hook(home.path(), tail);
                let _ = std::fs::remove_file(&counter);
                let mut inst = session(view);
                assert!(required(&launch(&mut inst)), "{label} {what}: refused");
                assert_eq!(hook_runs(&counter), 1, "{label} {what}");
                assert!(
                    required(&launch(&mut inst)),
                    "{label} {what}: refused again"
                );
                assert_eq!(hook_runs(&counter), 1, "{label} {what}: not run again");
            }

            // A failing hook surfaces its own error, not the login refusal.
            let counter = counting_before_start_hook(home.path(), "exit 3");
            let _ = std::fs::remove_file(&counter);
            let mut inst = session(view);
            let result = launch(&mut inst);
            assert!(!required(&result), "{label}: {result:?}");
            let message = format!("{:#}", result.unwrap_err());
            assert!(
                message.contains("before_start hook failed"),
                "{label}: {message}"
            );

            // Without a hook nothing runs and the refusal stands.
            crate::session::config::update_config(|global| global.host_hooks.before_start.clear())
                .unwrap();
            let mut inst = session(view);
            assert!(required(&launch(&mut inst)), "{label}: no hook");
            assert!(inst
                .sandbox_info
                .as_ref()
                .unwrap()
                .before_start_env
                .is_empty());

            // A usable login admits at once: the hook is not run for nothing.
            let counter = counting_before_start_hook(home.path(), "echo ANTHROPIC_API_KEY=sk-hook");
            let _ = std::fs::remove_file(&counter);
            write_shared_credential(home.path(), &usable_credential());
            let mut inst = session(view);
            assert!(launch(&mut inst).is_ok(), "{label}");
            assert_eq!(hook_runs(&counter), 0, "{label}: no hook at admission");
            write_shared_credential(home.path(), "{}");
        }
    }

    /// The hook sees the row the launch will run: admission reconciles the row from disk
    /// before it builds the hook's environment.
    #[test]
    #[serial_test::serial]
    #[serial_test::serial(hook_base)]
    fn sandbox_login_admission_builds_the_hook_environment_from_the_reconciled_row() {
        let (_hg, _, _base) = crate::hooks::test_support::BaseGuard::ready();
        let home = tempfile::TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_home(home.path());
        let _unset = crate::session::test_support::EnvGuard::unset(&[
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "ANTHROPIC_AUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "ANTHROPIC_API_KEY",
        ]);
        set_shared_credential_mode(SharedCredentialMode::SeparateLogin);
        write_shared_credential(home.path(), "{}");
        let project = home.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let seen = home.path().join("seen");
        let hook = format!(
            "echo \"marker=$TEST_MARKER\" >> {}; echo ANTHROPIC_API_KEY=sk-hook",
            crate::session::environment::shell_escape(&seen.to_string_lossy())
        );
        crate::session::config::update_config(|global| global.host_hooks.before_start = vec![hook])
            .unwrap();
        let profile = "w11-reconcile";
        let _registry = crate::tmux::status_rules::ProfileRegistryGuard::take(profile);
        let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
        let mut stored = sandboxed_instance("claude");
        stored.project_path = project.to_string_lossy().into_owned();
        stored.source_profile = profile.to_string();
        stored.sandbox_info.as_mut().unwrap().extra_env = Some(vec!["TEST_MARKER=disk".into()]);
        storage
            .update(|instances, _groups| {
                instances.push(stored.clone());
                Ok(())
            })
            .unwrap();
        // A copy that is stale against what a peer wrote.
        let mut live = stored.clone();
        live.sandbox_info.as_mut().unwrap().extra_env = Some(vec!["TEST_MARKER=stale".into()]);

        assert!(live.ensure_sandbox_launchable_for(None, &[]).is_ok());
        assert_eq!(
            std::fs::read_to_string(&seen).unwrap().trim(),
            "marker=disk",
            "the hook ran on the reconciled row"
        );
    }

    /// A resolution ages out and a restart's pre-check under the lifecycle locks never runs
    /// the hook; the resolution is the session's, so every copy of the row shares it and the
    /// come-up consumes it.
    #[test]
    #[serial_test::serial]
    #[serial_test::serial(hook_base)]
    fn sandbox_login_before_start_resolution_expires_is_shared_and_runs_no_hook_under_locks() {
        let (_hg, _, _base) = crate::hooks::test_support::BaseGuard::ready();
        let home = tempfile::TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_home(home.path());
        let _unset = crate::session::test_support::EnvGuard::unset(&[
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "ANTHROPIC_AUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "ANTHROPIC_API_KEY",
        ]);
        set_shared_credential_mode(SharedCredentialMode::SeparateLogin);
        write_shared_credential(home.path(), "{}");
        let project = home.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let counter = counting_before_start_hook(home.path(), "echo FOO=bar");
        let session = || {
            let mut inst = sandboxed_instance("claude");
            inst.project_path = project.to_string_lossy().into_owned();
            inst
        };
        let required = |result: anyhow::Result<()>| {
            result
                .err()
                .and_then(|e| e.downcast::<SandboxLoginRequired>().ok())
                .is_some()
        };

        // Under the locks no hook runs: an unresolved one is refused by the strict re-check
        // (a restart resolves its hook before the locks), never deferred.
        let mut inst = session();
        assert!(required(inst.ensure_sandbox_launchable_strict()));
        assert_eq!(hook_runs(&counter), 0, "no hook under the locks");
        assert!(required(inst.ensure_sandbox_launchable()));
        assert_eq!(hook_runs(&counter), 1);
        assert!(inst.ensure_sandbox_launchable_strict().is_err());
        assert_eq!(hook_runs(&counter), 1);

        // A resolution older than the limit is unresolved: the hook runs again.
        inst.expire_before_start_resolution();
        assert!(required(inst.ensure_sandbox_launchable()));
        assert_eq!(
            hook_runs(&counter),
            2,
            "an expired resolution is resolved again"
        );

        // Every copy of the row shares the session's resolution, and the come-up consumes it.
        let mut live = session();
        let mut copy = live.clone();
        assert!(required(copy.ensure_sandbox_launchable()));
        assert_eq!(hook_runs(&counter), 3);
        assert!(required(live.ensure_sandbox_launchable()));
        assert_eq!(hook_runs(&counter), 3, "another copy reuses the resolution");
        live.ensure_before_start_env(true).unwrap();
        assert_eq!(hook_runs(&counter), 3, "the come-up consumes it");
        assert!(required(live.ensure_sandbox_launchable()));
        assert_eq!(
            hook_runs(&counter),
            4,
            "consumed: the next admission resolves again"
        );
    }

    /// Pre-checks racing on one dormant session (sends, a switch, a launch: each on its own
    /// copy of the row) run the hook once between them, and so does the come-up that follows.
    #[test]
    #[serial_test::serial]
    #[serial_test::serial(hook_base)]
    fn sandbox_login_concurrent_pre_checks_run_the_before_start_hook_once() {
        let (_hg, _, _base) = crate::hooks::test_support::BaseGuard::ready();
        let home = tempfile::TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_home(home.path());
        let _unset = crate::session::test_support::EnvGuard::unset(&[
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "ANTHROPIC_AUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "ANTHROPIC_API_KEY",
        ]);
        set_shared_credential_mode(SharedCredentialMode::SeparateLogin);
        write_shared_credential(home.path(), "{}");
        let project = home.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        // The hook takes a moment, so every thread is inside the check while it runs.
        let counter =
            counting_before_start_hook(home.path(), "sleep 0.5; echo ANTHROPIC_API_KEY=sk-hook");
        let mut row = sandboxed_instance("claude");
        row.project_path = project.to_string_lossy().into_owned();
        row.view = View::Structured;

        let threads: Vec<_> = (0..4)
            .map(|n| {
                let mut copy = row.clone();
                std::thread::spawn(move || match n {
                    0 => copy.ensure_sandbox_launchable_for(Some("codex"), &[]),
                    1 => copy.ensure_sandbox_launchable_for(None, &[]),
                    _ => copy.ensure_sandbox_launchable(),
                })
            })
            .collect();
        for thread in threads {
            assert!(thread.join().unwrap().is_ok(), "every copy is admitted");
        }
        assert_eq!(hook_runs(&counter), 1, "the hook ran once for all of them");

        let mut comes_up = row.clone();
        comes_up.ensure_before_start_env(true).unwrap();
        assert_eq!(hook_runs(&counter), 1, "the come-up used the resolution");
        assert_eq!(
            comes_up.sandbox_info.as_ref().unwrap().before_start_env,
            vec![("ANTHROPIC_API_KEY".to_string(), "sk-hook".to_string())]
        );
    }

    /// A cache an earlier come-up left on the row is no auth route: the hook may emit
    /// something else now. The gate re-resolves instead of trusting it, and a changed hook
    /// never reuses an older resolution.
    #[test]
    #[serial_test::serial]
    #[serial_test::serial(hook_base)]
    fn sandbox_login_gate_does_not_trust_a_stale_hook_cache_or_a_changed_hook() {
        let (_hg, _, _base) = crate::hooks::test_support::BaseGuard::ready();
        let home = tempfile::TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_home(home.path());
        let _unset = crate::session::test_support::EnvGuard::unset(&[
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "ANTHROPIC_AUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "ANTHROPIC_API_KEY",
        ]);
        set_shared_credential_mode(SharedCredentialMode::SeparateLogin);
        write_shared_credential(home.path(), "{}");
        let project = home.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let session = || {
            let mut inst = sandboxed_instance("claude");
            inst.project_path = project.to_string_lossy().into_owned();
            // What an earlier come-up minted.
            inst.sandbox_info.as_mut().unwrap().before_start_env =
                vec![("ANTHROPIC_API_KEY".to_string(), "sk-old".to_string())];
            inst
        };
        let required = |result: anyhow::Result<()>| {
            result
                .err()
                .and_then(|e| e.downcast::<SandboxLoginRequired>().ok())
                .is_some()
        };

        // The hook now emits nothing useful: the stale key does not admit the launch.
        let counter = counting_before_start_hook(home.path(), "echo FOO=bar");
        let _ = std::fs::remove_file(&counter);
        let mut inst = session();
        assert!(required(inst.ensure_sandbox_launchable()), "stale cache");
        assert_eq!(hook_runs(&counter), 1, "re-resolved, not trusted");

        // The hook now emits a different key: that one is the launch's.
        let counter = counting_before_start_hook(home.path(), "echo ANTHROPIC_API_KEY=sk-new");
        let _ = std::fs::remove_file(&counter);
        let mut inst = session();
        assert!(inst.ensure_sandbox_launchable().is_ok());
        assert_eq!(hook_runs(&counter), 1);
        assert_eq!(
            inst.sandbox_info.as_ref().unwrap().before_start_env,
            vec![("ANTHROPIC_API_KEY".to_string(), "sk-new".to_string())]
        );

        // A resolution of one hook is not reused once the hook changed.
        let counter = counting_before_start_hook(home.path(), "echo FOO=bar");
        let _ = std::fs::remove_file(&counter);
        let mut inst = session();
        assert!(required(inst.ensure_sandbox_launchable()));
        assert_eq!(hook_runs(&counter), 1);
        let counter = counting_before_start_hook(home.path(), "echo ANTHROPIC_API_KEY=sk-changed");
        let _ = std::fs::remove_file(&counter);
        assert!(
            inst.ensure_sandbox_launchable().is_ok(),
            "the changed hook ran"
        );
        assert_eq!(hook_runs(&counter), 1);
    }

    fn hook_prelude(home: &std::path::Path) {
        set_shared_credential_mode(SharedCredentialMode::SeparateLogin);
        write_shared_credential(home, "{}");
    }

    /// A hook that never exits is cut off at its deadline (its process tree killed), and a
    /// failed run says only that it failed: the error and the logs carry neither the command
    /// nor the hook's output, which may hold tokens. The slot stays unresolved, so a later
    /// attempt resolves.
    #[test]
    #[serial_test::serial]
    #[serial_test::serial(hook_base)]
    fn sandbox_login_before_start_hook_is_bounded_and_its_failure_is_opaque() {
        let (_hg, _, _base) = crate::hooks::test_support::BaseGuard::ready();
        let home = tempfile::TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_home(home.path());
        let _unset = crate::session::test_support::EnvGuard::unset(&[
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "ANTHROPIC_AUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "ANTHROPIC_API_KEY",
        ]);
        hook_prelude(home.path());
        let project = home.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let logs = crate::session::test_support::LogCapture::start();
        let session = || {
            let mut inst = sandboxed_instance("claude");
            inst.project_path = project.to_string_lossy().into_owned();
            inst
        };

        // Never exits: cut off at the deadline.
        let counter = counting_before_start_hook(
            home.path(),
            "echo token=sekret-token >&2; echo \"K=$HOOK_ONLY_VALUE\"; sleep 30",
        );
        let _ = std::fs::remove_file(&counter);
        let started = std::time::Instant::now();
        let error = {
            let _deadline =
                crate::session::HookTimeoutScope::new(std::time::Duration::from_millis(400));
            session().ensure_sandbox_launchable().unwrap_err()
        };
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "cut off"
        );
        assert_eq!(format!("{error:#}"), "the before_start hook timed out");

        // Exits unsuccessfully: only the class.
        let counter =
            counting_before_start_hook(home.path(), "echo token=sekret-token >&2; set -x; exit 3");
        let _ = std::fs::remove_file(&counter);
        let error = session().ensure_sandbox_launchable().unwrap_err();
        assert_eq!(format!("{error:#}"), "the before_start hook failed");
        let logged = logs.contents();
        for text in [format!("{error:?}"), logged] {
            assert!(!text.contains("sekret-token"), "{text}");
            assert!(!text.contains("exit 3"), "{text}");
            assert!(!text.contains("echo token"), "{text}");
        }

        // The slot was left unresolved: a working hook resolves on the next attempt.
        let counter = counting_before_start_hook(home.path(), "echo ANTHROPIC_API_KEY=sk-hook");
        let _ = std::fs::remove_file(&counter);
        assert!(session().ensure_sandbox_launchable().is_ok());
    }

    /// A caller waits for another's hook run only as long as its own deadline plus a margin:
    /// one blocked behind a run that waits for it (a hook calling back into the daemon about
    /// its own session) times out instead of deadlocking, and the holder is cut off by its
    /// own deadline. The slot stays consistent for a later retry.
    #[test]
    #[serial_test::serial]
    #[serial_test::serial(hook_base)]
    fn sandbox_login_slot_waiters_are_bounded_and_a_timeout_leaves_the_slot_retryable() {
        let (_hg, _, _base) = crate::hooks::test_support::BaseGuard::ready();
        let home = tempfile::TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_home(home.path());
        let _unset = crate::session::test_support::EnvGuard::unset(&[
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "ANTHROPIC_AUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "ANTHROPIC_API_KEY",
        ]);
        hook_prelude(home.path());
        let project = home.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let counter = counting_before_start_hook(home.path(), "sleep 30");
        let _ = std::fs::remove_file(&counter);
        let mut row = sandboxed_instance("claude");
        row.project_path = project.to_string_lossy().into_owned();

        let holder = {
            let mut copy = row.clone();
            std::thread::spawn(move || {
                let _deadline =
                    crate::session::HookTimeoutScope::new(std::time::Duration::from_secs(3));
                copy.ensure_sandbox_launchable()
            })
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while hook_runs(&counter) == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(hook_runs(&counter), 1, "the holder's hook is running");

        let started = std::time::Instant::now();
        let waiter = {
            let _deadline =
                crate::session::HookTimeoutScope::new(std::time::Duration::from_millis(200));
            row.clone().ensure_sandbox_launchable().unwrap_err()
        };
        assert!(
            started.elapsed() < std::time::Duration::from_millis(2000),
            "the waiter is bounded by its own deadline: {:?}",
            started.elapsed()
        );
        assert_eq!(
            format!("{waiter:#}"),
            "the before_start hook is still running for this session"
        );
        assert_eq!(hook_runs(&counter), 1, "the waiter ran nothing");

        let held = holder.join().unwrap().unwrap_err();
        assert_eq!(format!("{held:#}"), "the before_start hook timed out");

        let counter = counting_before_start_hook(home.path(), "echo ANTHROPIC_API_KEY=sk-hook");
        let _ = std::fs::remove_file(&counter);
        assert!(
            row.clone().ensure_sandbox_launchable().is_ok(),
            "retry resolves"
        );
    }

    /// When the come-up has to mint itself because the resolution its admission relied on has
    /// expired, the new environment is gated before anything is created: a hook that now
    /// emits no key refuses the launch cleanly. The mint leaves the slot empty.
    #[test]
    #[serial_test::serial]
    #[serial_test::serial(hook_base)]
    fn sandbox_login_come_up_regates_when_it_has_to_mint_again() {
        let (_hg, _, _base) = crate::hooks::test_support::BaseGuard::ready();
        let home = tempfile::TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_home(home.path());
        let _unset = crate::session::test_support::EnvGuard::unset(&[
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "ANTHROPIC_AUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "ANTHROPIC_API_KEY",
        ]);
        hook_prelude(home.path());
        let project = home.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let mut inst = sandboxed_instance("claude");
        inst.project_path = project.to_string_lossy().into_owned();

        let counter = counting_before_start_hook(home.path(), "echo ANTHROPIC_API_KEY=sk-hook");
        let _ = std::fs::remove_file(&counter);
        assert!(inst.ensure_sandbox_launchable().is_ok());
        // The image pull took longer than the resolution lives, and the hook now emits no key.
        inst.expire_before_start_resolution();
        let counter = counting_before_start_hook(home.path(), "echo FOO=bar");
        let _ = std::fs::remove_file(&counter);
        let error = inst.ensure_before_start_env(true).unwrap_err();
        assert!(
            error.downcast_ref::<SandboxLoginRequired>().is_some(),
            "{error:#}"
        );
        assert_eq!(hook_runs(&counter), 1);
        // The slot is empty: the next admission resolves again.
        assert!(inst.ensure_sandbox_launchable().is_err());
        assert_eq!(hook_runs(&counter), 2, "nothing was left filled");

        // A come-up whose admission needed no hook mints without re-gating (an admission can
        // rest on a request-only key the come-up cannot see).
        let mut other = sandboxed_instance("claude");
        other.project_path = project.to_string_lossy().into_owned();
        assert!(other.ensure_before_start_env(true).is_ok());
    }

    /// The come-up's own mint and a resolver never run the hook in parallel: the mint holds
    /// the slot lock, so the resolver runs after it, not beside it.
    #[test]
    #[serial_test::serial]
    #[serial_test::serial(hook_base)]
    fn sandbox_login_come_up_mint_and_resolver_never_run_the_hook_in_parallel() {
        let (_hg, _, _base) = crate::hooks::test_support::BaseGuard::ready();
        let home = tempfile::TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_home(home.path());
        let _unset = crate::session::test_support::EnvGuard::unset(&[
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "ANTHROPIC_AUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "ANTHROPIC_API_KEY",
        ]);
        hook_prelude(home.path());
        let project = home.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let counter = home.path().join("hook-runs");
        let hook = format!(
            "echo run >> {c}; sleep 0.5; echo end >> {c}; echo ANTHROPIC_API_KEY=sk-hook",
            c = crate::session::environment::shell_escape(&counter.to_string_lossy())
        );
        crate::session::config::update_config(|global| global.host_hooks.before_start = vec![hook])
            .unwrap();
        let mut row = sandboxed_instance("claude");
        row.project_path = project.to_string_lossy().into_owned();

        let come_up = {
            let mut copy = row.clone();
            std::thread::spawn(move || copy.ensure_before_start_env(true))
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while hook_runs(&counter) == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let resolver = {
            let mut copy = row.clone();
            std::thread::spawn(move || copy.ensure_sandbox_launchable())
        };
        come_up.join().unwrap().unwrap();
        resolver.join().unwrap().unwrap();
        assert_eq!(
            std::fs::read_to_string(&counter)
                .unwrap()
                .lines()
                .collect::<Vec<_>>(),
            ["run", "end", "run", "end"],
            "the runs did not overlap"
        );
    }

    /// A resolved environment nothing will consume is forgotten with its session (trashed,
    /// archived, deleted) and with its hook (removed from the configuration).
    #[test]
    #[serial_test::serial]
    #[serial_test::serial(hook_base)]
    fn sandbox_login_resolution_is_dropped_with_the_session_or_the_hook() {
        let (_hg, _, _base) = crate::hooks::test_support::BaseGuard::ready();
        let home = tempfile::TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_home(home.path());
        let _unset = crate::session::test_support::EnvGuard::unset(&[
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "ANTHROPIC_AUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "ANTHROPIC_API_KEY",
        ]);
        hook_prelude(home.path());
        let project = home.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let counter = counting_before_start_hook(home.path(), "echo ANTHROPIC_API_KEY=sk-hook");
        let session = || {
            let mut inst = sandboxed_instance("claude");
            inst.project_path = project.to_string_lossy().into_owned();
            inst
        };
        type Forget = (&'static str, fn(&mut Instance));
        let forgets: [Forget; 3] = [
            ("trash", |i| i.trash()),
            ("archive", |i| i.archive()),
            ("delete", |i| {
                crate::session::forget_before_start_resolution(&i.id.clone())
            }),
        ];
        for (what, forget) in forgets {
            let _ = std::fs::remove_file(&counter);
            let mut inst = session();
            assert!(inst.ensure_sandbox_launchable().is_ok(), "{what}");
            assert!(inst.ensure_sandbox_launchable().is_ok(), "{what}");
            assert_eq!(hook_runs(&counter), 1, "{what}: resolved once");
            forget(&mut inst);
            assert!(inst.ensure_sandbox_launchable().is_ok(), "{what}");
            assert_eq!(hook_runs(&counter), 2, "{what}: nothing was kept");
        }

        // The hook is removed, then configured again: nothing of the old run survives.
        let _ = std::fs::remove_file(&counter);
        let mut inst = session();
        assert!(inst.ensure_sandbox_launchable().is_ok());
        crate::session::config::update_config(|global| global.host_hooks.before_start.clear())
            .unwrap();
        inst.ensure_before_start_env(true).unwrap();
        let counter = counting_before_start_hook(home.path(), "echo ANTHROPIC_API_KEY=sk-hook");
        assert_eq!(hook_runs(&counter), 1, "the earlier run's count");
        assert!(inst.ensure_sandbox_launchable().is_ok());
        assert_eq!(hook_runs(&counter), 2, "resolved again, not reused");
    }

    /// A restart re-checks strictly under the locks: a resolution that expired or was consumed
    /// since the early one refuses, and no hook runs there.
    #[test]
    #[serial_test::serial]
    #[serial_test::serial(hook_base)]
    fn sandbox_login_restart_recheck_refuses_what_expired_or_was_consumed() {
        let (_hg, _, _base) = crate::hooks::test_support::BaseGuard::ready();
        let home = tempfile::TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_home(home.path());
        let _unset = crate::session::test_support::EnvGuard::unset(&[
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "ANTHROPIC_AUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "ANTHROPIC_API_KEY",
        ]);
        hook_prelude(home.path());
        let project = home.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let counter = counting_before_start_hook(home.path(), "echo ANTHROPIC_API_KEY=sk-hook");
        let _ = std::fs::remove_file(&counter);
        let mut inst = sandboxed_instance("claude");
        inst.project_path = project.to_string_lossy().into_owned();
        let refused = |inst: &Instance| {
            inst.ensure_sandbox_launchable_strict()
                .err()
                .and_then(|e| e.downcast::<SandboxLoginRequired>().ok())
                .is_some()
        };

        assert!(refused(&inst), "unresolved: refused, not deferred");
        assert!(inst.ensure_sandbox_launchable().is_ok());
        assert!(!refused(&inst), "resolved: admitted");
        inst.expire_before_start_resolution();
        assert!(refused(&inst), "expired while waiting for the locks");
        assert!(inst.ensure_sandbox_launchable().is_ok());
        inst.ensure_before_start_env(true).unwrap();
        assert!(refused(&inst), "consumed by a concurrent come-up");
        assert_eq!(hook_runs(&counter), 2, "the re-check ran no hook");
    }

    fn hook_env_unset() -> crate::session::test_support::EnvGuard {
        crate::session::test_support::EnvGuard::unset(&[
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "ANTHROPIC_AUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "ANTHROPIC_API_KEY",
        ])
    }

    fn required_error(result: anyhow::Result<()>) -> bool {
        result
            .err()
            .and_then(|e| e.downcast::<SandboxLoginRequired>().ok())
            .is_some()
    }

    /// The come-up gates again with what its admission ran with: a request-only key and the
    /// target agent of a switch are part of the launch, so a come-up that has to mint
    /// (expired resolution, hook that now emits nothing) does not refuse a launch they admit.
    #[test]
    #[serial_test::serial]
    #[serial_test::serial(hook_base)]
    fn sandbox_login_come_up_regate_carries_the_request_key_and_the_target_agent() {
        let (_hg, _, _base) = crate::hooks::test_support::BaseGuard::ready();
        let home = tempfile::TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_home(home.path());
        let _unset = hook_env_unset();
        hook_prelude(home.path());
        let project = home.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let _counter = counting_before_start_hook(home.path(), "echo FOO=bar");
        let session = || {
            let mut inst = sandboxed_instance("claude");
            inst.project_path = project.to_string_lossy().into_owned();
            inst.view = View::Structured;
            inst.agent_name = Some("codex".to_string());
            inst
        };

        // (a) A request-only key admits the launch with no hook run; the come-up that mints
        // after the old empty resolution expired still sees the key.
        let mut inst = session();
        assert!(required_error(
            inst.ensure_sandbox_launchable_for(None, &[])
        ));
        inst.expire_before_start_resolution();
        let request = [("ANTHROPIC_API_KEY".to_string(), "sk-request".to_string())];
        assert!(inst.ensure_sandbox_launchable_for(None, &request).is_ok());
        assert!(
            inst.ensure_before_start_env(true).is_ok(),
            "the request's key is part of the launch the come-up gates"
        );

        // (b) A switch's target agent, not the row's stored one, decides which host variables
        // cross: the daemon's key reaches claude, not codex.
        let _host =
            crate::session::test_support::EnvGuard::set(&[("ANTHROPIC_API_KEY", "sk-host")]);
        let mut switching = session();
        assert!(required_error(
            switching.ensure_sandbox_launchable_for(None, &[])
        ));
        switching.expire_before_start_resolution();
        assert!(switching
            .ensure_sandbox_launchable_for(Some("claude"), &[])
            .is_ok());
        assert!(
            switching.ensure_before_start_env(true).is_ok(),
            "the come-up gates on the target agent"
        );
    }

    /// A come-up that mints because a concurrent come-up consumed the resolution gates again:
    /// a hook that emitted a key once and nothing now does not launch with no auth route.
    #[test]
    #[serial_test::serial]
    #[serial_test::serial(hook_base)]
    fn sandbox_login_come_up_regates_when_the_resolution_was_consumed() {
        let (_hg, _, _base) = crate::hooks::test_support::BaseGuard::ready();
        let home = tempfile::TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_home(home.path());
        let _unset = hook_env_unset();
        hook_prelude(home.path());
        let project = home.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let once = home.path().join("emitted-once");
        let _counter = counting_before_start_hook(
            home.path(),
            &format!(
                "if [ ! -e {f} ]; then touch {f}; echo ANTHROPIC_API_KEY=sk-once; fi",
                f = crate::session::environment::shell_escape(&once.to_string_lossy())
            ),
        );
        let mut row = sandboxed_instance("claude");
        row.project_path = project.to_string_lossy().into_owned();
        let mut other = row.clone();

        assert!(row.ensure_sandbox_launchable().is_ok(), "the key admits it");
        other.ensure_before_start_env(true).unwrap();
        let error = row.ensure_before_start_env(true).unwrap_err();
        assert!(
            error.downcast_ref::<SandboxLoginRequired>().is_some(),
            "second mint without a key: {error:#}"
        );
    }

    /// An already-running container comes up nothing: the session's resolution is not
    /// consumed, so the next refused admission does not run the hook again.
    #[test]
    #[serial_test::serial]
    #[serial_test::serial(hook_base)]
    fn sandbox_login_running_container_leaves_the_resolution_alone() {
        let (_hg, _, _base) = crate::hooks::test_support::BaseGuard::ready();
        let home = tempfile::TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_home(home.path());
        let _unset = hook_env_unset();
        hook_prelude(home.path());
        let project = home.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let counter = counting_before_start_hook(home.path(), "echo FOO=bar");
        let _ = std::fs::remove_file(&counter);
        let mut inst = sandboxed_instance("claude");
        inst.project_path = project.to_string_lossy().into_owned();

        assert!(required_error(inst.ensure_sandbox_launchable()));
        assert_eq!(hook_runs(&counter), 1);
        inst.ensure_before_start_env(false).unwrap();
        assert!(required_error(inst.ensure_sandbox_launchable()));
        assert_eq!(hook_runs(&counter), 1, "attaching consumed nothing");
    }

    /// All commands of a resolution share one deadline, and a waiter does not fail while
    /// the holder finishes within it.
    #[test]
    #[serial_test::serial]
    #[serial_test::serial(hook_base)]
    fn sandbox_login_before_start_commands_share_one_deadline() {
        let (_hg, _, _base) = crate::hooks::test_support::BaseGuard::ready();
        let home = tempfile::TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_home(home.path());
        let _unset = hook_env_unset();
        hook_prelude(home.path());
        let project = home.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        crate::session::config::update_config(|global| {
            global.host_hooks.before_start = vec![
                "sleep 0.4; echo A=1".to_string(),
                "sleep 0.4; echo ANTHROPIC_API_KEY=sk-hook".to_string(),
            ];
        })
        .unwrap();
        let mut row = sandboxed_instance("claude");
        row.project_path = project.to_string_lossy().into_owned();

        // Two commands of 0.4 s do not fit one 0.6 s deadline, though each fits alone.
        let error = {
            let _deadline =
                crate::session::HookTimeoutScope::new(std::time::Duration::from_millis(600));
            row.clone().ensure_sandbox_launchable().unwrap_err()
        };
        assert_eq!(format!("{error:#}"), "the before_start hook timed out");

        // A healthy holder inside the deadline: the waiter takes its result.
        let holder = {
            let mut copy = row.clone();
            std::thread::spawn(move || {
                let _deadline =
                    crate::session::HookTimeoutScope::new(std::time::Duration::from_secs(5));
                copy.ensure_sandbox_launchable()
            })
        };
        std::thread::sleep(std::time::Duration::from_millis(150));
        let waiter = {
            let _deadline =
                crate::session::HookTimeoutScope::new(std::time::Duration::from_secs(5));
            row.clone().ensure_sandbox_launchable()
        };
        assert!(holder.join().unwrap().is_ok());
        assert!(waiter.is_ok(), "{waiter:?}");
    }

    /// `Debug` of a row never prints the hook's environment or the request's.
    #[test]
    #[serial_test::serial]
    #[serial_test::serial(hook_base)]
    fn sandbox_login_debug_output_redacts_the_hook_and_request_environment() {
        let (_hg, _, _base) = crate::hooks::test_support::BaseGuard::ready();
        let home = tempfile::TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_home(home.path());
        let _unset = hook_env_unset();
        hook_prelude(home.path());
        let mut inst = sandboxed_instance("claude");
        inst.view = View::Structured;
        inst.sandbox_info.as_mut().unwrap().before_start_env = vec![(
            "ANTHROPIC_API_KEY".to_string(),
            "sekret-hook-key".to_string(),
        )];
        let request = [(
            "ANTHROPIC_API_KEY".to_string(),
            "sekret-request-key".to_string(),
        )];
        let _ = inst.ensure_sandbox_launchable_for(None, &request);
        let printed = format!("{inst:?} {:?}", inst.sandbox_info);
        assert!(!printed.contains("sekret-hook-key"), "{printed}");
        assert!(!printed.contains("sekret-request-key"), "{printed}");
    }

    /// A session deleted while a caller waits on its slot behind a running resolution: the
    /// waiter does not take the deleted session's result, and the resolver does not store it.
    #[test]
    #[serial_test::serial]
    #[serial_test::serial(hook_base)]
    fn sandbox_login_delete_during_a_resolution_wins() {
        let (_hg, _, _base) = crate::hooks::test_support::BaseGuard::ready();
        let home = tempfile::TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_home(home.path());
        let _unset = hook_env_unset();
        hook_prelude(home.path());
        let project = home.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let counter =
            counting_before_start_hook(home.path(), "sleep 0.8; echo ANTHROPIC_API_KEY=sk-hook");
        let _ = std::fs::remove_file(&counter);
        let mut row = sandboxed_instance("claude");
        row.project_path = project.to_string_lossy().into_owned();
        let id = row.id.clone();

        let holder = {
            let mut copy = row.clone();
            std::thread::spawn(move || copy.ensure_sandbox_launchable())
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while hook_runs(&counter) == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let waiter = {
            let mut copy = row.clone();
            std::thread::spawn(move || copy.ensure_sandbox_launchable())
        };
        std::thread::sleep(std::time::Duration::from_millis(150));
        crate::session::forget_before_start_resolution(&id);
        assert!(holder.join().unwrap().is_ok());
        assert!(waiter.join().unwrap().is_ok());
        assert_eq!(
            hook_runs(&counter),
            2,
            "the waiter resolved afresh instead of taking the deleted session's result"
        );
    }

    /// A restart re-checks with a safety margin: a resolution about to expire is refused
    /// before anything is stopped, and the early resolution renews it.
    #[test]
    #[serial_test::serial]
    #[serial_test::serial(hook_base)]
    fn sandbox_login_restart_requires_a_resolution_with_margin_and_expiry_drops_it() {
        let (_hg, _, _base) = crate::hooks::test_support::BaseGuard::ready();
        let home = tempfile::TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_home(home.path());
        let _unset = hook_env_unset();
        hook_prelude(home.path());
        let project = home.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let counter = counting_before_start_hook(home.path(), "echo ANTHROPIC_API_KEY=sk-hook");
        let _ = std::fs::remove_file(&counter);
        let mut inst = sandboxed_instance("claude");
        inst.project_path = project.to_string_lossy().into_owned();

        assert!(inst.ensure_sandbox_launchable().is_ok());
        assert!(inst.ensure_sandbox_launchable_strict().is_ok());
        inst.age_before_start_resolution(std::time::Duration::from_secs(30));
        assert!(
            required_error(inst.ensure_sandbox_launchable_strict()),
            "30 s left is under the restart's margin"
        );
        inst.resolve_before_start_for_restart().unwrap();
        assert_eq!(hook_runs(&counter), 2, "the early resolution renewed it");
        assert!(inst.ensure_sandbox_launchable_strict().is_ok());

        inst.expire_before_start_resolution();
        assert!(
            inst.holds_before_start_resolution(),
            "expired but not yet found"
        );
        let _ = inst.ensure_sandbox_launchable_strict();
        assert!(
            !inst.holds_before_start_resolution(),
            "an expired resolution is dropped when found"
        );
    }

    /// The come-up resolves its hook, and gates on it again if it has to mint, before the
    /// destructive steps (image pull, container creation or removal): a refusal leaves the
    /// runtime untouched.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    #[serial_test::serial(hook_base)]
    fn sandbox_login_come_up_refuses_before_any_destructive_step() {
        use std::os::unix::fs::PermissionsExt;
        let (_hg, _, _base) = crate::hooks::test_support::BaseGuard::ready();
        let home = tempfile::TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_home(home.path());
        let _unset = hook_env_unset();
        hook_prelude(home.path());
        let project = home.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let bin = home.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let calls = home.path().join("runtime-calls");
        // No such container; every other call fails, as a missing image would.
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{calls}'\n\
             if [ \"$1\" = container ] && [ \"$2\" = inspect ]; then echo 'Error: No such container: c' >&2; exit 1; fi\n\
             case \"$*\" in --version|info|'system status') exit 0;; esac\n\
             echo 'permission denied' >&2\nexit 1\n",
            calls = calls.display()
        );
        for binary in ["docker", "podman", "container"] {
            let path = bin.join(binary);
            std::fs::write(&path, &script).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let _path = crate::session::test_support::path_prepended(&bin);
        let _counter = counting_before_start_hook(home.path(), "echo ANTHROPIC_API_KEY=sk-hook");
        let mut inst = sandboxed_instance("claude");
        inst.project_path = project.to_string_lossy().into_owned();
        assert!(inst.ensure_sandbox_launchable().is_ok());
        inst.expire_before_start_resolution();
        let _no_key = counting_before_start_hook(home.path(), "echo FOO=bar");

        let error = match inst.get_container_for_instance() {
            Ok(_) => panic!("the come-up must be refused"),
            Err(error) => error,
        };
        assert!(
            error.downcast_ref::<SandboxLoginRequired>().is_some(),
            "{error:#}"
        );
        let recorded = std::fs::read_to_string(&calls).unwrap_or_default();
        for step in ["pull", "image", "create", "run", "rm", "start"] {
            assert!(
                !recorded
                    .lines()
                    .any(|l| l.split_whitespace().next() == Some(step)),
                "{step} ran before the refusal: {recorded}"
            );
        }
    }

    fn set_container_runtime(runtime: crate::session::config::ContainerRuntimeName) {
        crate::session::config::update_config(|c| c.sandbox.container_runtime = runtime).unwrap();
    }

    /// `separate_login` on Apple Container refuses the launches that would wait for the
    /// login, since the helper that performs it does not run there. The refusal is typed,
    /// never a hold, and only touches launches that need the shared credential.
    #[test]
    #[serial_test::serial]
    #[serial_test::serial(hook_base)]
    fn sandbox_login_gate_refuses_the_mode_on_apple_container() {
        use crate::session::config::ContainerRuntimeName::{AppleContainer, Docker, Podman};
        let (_hg, _, _base) = crate::hooks::test_support::BaseGuard::ready();
        let home = tempfile::TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_home(home.path());
        let _unset = crate::session::test_support::EnvGuard::unset(&[
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "ANTHROPIC_AUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "ANTHROPIC_API_KEY",
        ]);
        set_shared_credential_mode(SharedCredentialMode::SeparateLogin);
        write_shared_credential(home.path(), "{}");
        let mut claude = sandboxed_instance("claude");
        let refused = |inst: &mut Instance| {
            inst.ensure_sandbox_launchable()
                .unwrap_err()
                .downcast_ref::<SandboxLoginUnsupported>()
                .is_some()
        };

        set_container_runtime(AppleContainer);
        assert!(refused(&mut claude), "the OAuth launch is refused");
        assert_eq!(
            claude.ensure_sandbox_login(),
            Ok(()),
            "a refusal is not a hold"
        );
        assert_eq!(
            SandboxLoginUnsupported.to_string(),
            "sandbox.shared_credential is separate_login, which is not supported with Apple Container; use host_seed or Docker/Podman"
        );

        // A launch that does not use the shared credential proceeds.
        let mut keyed = sandboxed_instance("claude");
        keyed.view = View::Structured;
        let request = [("ANTHROPIC_API_KEY".to_string(), "sk-request".to_string())];
        assert!(keyed.ensure_sandbox_launchable_for(None, &request).is_ok());
        let mut bedrock = {
            let mut inst = sandboxed_instance("claude");
            if let Some(sandbox) = inst.sandbox_info.as_mut() {
                sandbox.extra_env = Some(vec!["CLAUDE_CODE_USE_BEDROCK=1".to_string()]);
            }
            inst
        };
        assert!(bedrock.ensure_sandbox_launchable().is_ok());
        // An agent without a sandbox login is not gated.
        assert!(sandboxed_instance("codex")
            .ensure_sandbox_launchable()
            .is_ok());

        // A login the shared file already holds needs no helper.
        write_shared_credential(home.path(), &usable_credential());
        assert!(claude.ensure_sandbox_launchable().is_ok());
        write_shared_credential(home.path(), "{}");

        // `host_seed` is unaffected.
        set_shared_credential_mode(SharedCredentialMode::HostSeed);
        assert!(claude.ensure_sandbox_launchable().is_ok());
        set_shared_credential_mode(SharedCredentialMode::SeparateLogin);

        // Docker and Podman hold the launch as before.
        for runtime in [Docker, Podman] {
            set_container_runtime(runtime);
            let error = claude.ensure_sandbox_launchable().unwrap_err();
            assert_eq!(
                error.downcast_ref::<SandboxLoginRequired>(),
                Some(&crate::session::test_support::login_required_under(
                    "claude",
                    home.path()
                )),
                "{runtime:?}"
            );
            assert!(claude.ensure_sandbox_login().is_err(), "{runtime:?}");
        }
    }

    /// Regression for issue #2414: a sandboxed worktree session's
    /// `container_workdir()` must stay pinned to what the container was created
    /// with, even after the host worktree's git linkage breaks.
    ///
    /// When the worktree's admin entry under `<main>/.git/worktrees/<name>` is
    /// pruned, the `.git` file's gitdir no longer resolves, `compute_volume_paths`
    /// can't find the main repo, and it silently collapses to
    /// `/workspace/<basename>` (a path the container never mounted), so a
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
            provider: None,
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
        instance.sandbox_info = Some(SandboxInfo {
            provider: None,
            enabled: true,
            container_id: None,
            image: "test:latest".to_string(),
            container_name: "diagnostic".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
        });
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
        let project = temp.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let bin = temp.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let calls_path = temp.path().join("runtime-calls");
        let label_path = temp.path().join("tool-label");
        let removed_path = temp.path().join("removed");
        // A stopped container carrying the tool label read from `label_path`.
        // Removal makes it absent; every other call fails, so the launch stops
        // before tmux.
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{calls}'\n\
             if [ \"$1\" = rm ]; then touch '{removed}'; exit 0; fi\n\
             if [ \"$1\" = container ] && [ \"$2\" = inspect ]; then\n\
             if [ -e '{removed}' ]; then echo 'Error: No such container: c' >&2; exit 1; fi\n\
             if [ \"$#\" -eq 3 ]; then echo '[{{\"Id\":\"fake\",\"State\":{{\"Running\":false}},\"Mounts\":[]}}]'; exit 0; fi\n\
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
        // The expected store is the reconciled agent config root admitted before launch.
        let cases = [
            (("codex", ""), "claude", Disk::Absent, 1, Some(".codex")),
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
            (
                ("claude", ""),
                "claude",
                Disk::Row("codex", ""),
                1,
                Some(".codex"),
            ),
            // A status-only alias is not an execution identity, so this row's
            // container label is its tool name and the reuse path refreshes no
            // store. A wrapper or a built-in tool is what carries a store.
            (
                ("alias-a", "claude"),
                "alias-b",
                Disk::Row("alias-b", "codex"),
                0,
                None,
            ),
            // Same contract: the alias-only row labels its container "alias-a"
            // and therefore reuses it instead of rebuilding.
            (("alias-a", "codex"), "alias-a", Disk::Absent, 0, None),
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
            let mut instance = Instance::new("tool label", project.to_str().unwrap());
            instance.tool = tool.to_string();
            instance.detect_as = detect_as.to_string();
            instance.source_profile = profile.to_string();
            instance.sandbox_info = Some(SandboxInfo {
                provider: None,
                enabled: true,
                container_id: None,
                image: "test:latest".to_string(),
                container_name: "tool-label".to_string(),
                extra_env: None,
                custom_instruction: None,
                before_start_env: Vec::new(),
                container_workdir: None,
            });
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
            let mut instance = Instance::new("tool label", project.to_str().unwrap());
            instance.tool = tool.to_string();
            instance.detect_as = detect_as.to_string();
            instance.source_profile = profile.to_string();
            instance.sandbox_info = Some(SandboxInfo {
                provider: None,
                enabled: true,
                container_id: None,
                image: "test:latest".to_string(),
                container_name: "tool-label".to_string(),
                extra_env: None,
                custom_instruction: None,
                before_start_env: Vec::new(),
                container_workdir: None,
            });
            assert_eq!(
                instance.build_container_config().unwrap().agent_tool,
                instance.container_agent_identity().unwrap(),
                "{tool}/{detect_as}"
            );
        }
    }

    /// #4205: an archive or trash that completes while the `before_start` hook runs stops the
    /// container come-up, for both the restart and the create branch.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn admitted_container_start_refuses_a_row_dismissed_during_the_hook() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let project = temp.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let bin = temp.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let calls_path = temp.path().join("runtime-calls");
        let absent_path = temp.path().join("container-absent");
        // A stopped `opencode` container (no shared credential mounts to match) unless
        // `absent_path` exists. Bring-up calls are logged and fail, so the launch stops before tmux.
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{calls}'\n\
             case \"$1\" in start|run|create) echo 'permission denied' >&2; exit 1;; image) exit 0;; esac\n\
             if [ \"$1\" = container ] && [ \"$2\" = inspect ]; then\n\
             if [ -e '{absent}' ]; then echo 'Error: No such container: c' >&2; exit 1; fi\n\
             if [ \"$#\" -eq 3 ]; then echo '[{{\"Id\":\"fake\",\"State\":{{\"Running\":false}},\"Mounts\":[]}}]'; exit 0; fi\n\
             case \"$*\" in\n\
             *agent-tool*) ;;\n\
             *sandbox-store-generation*) echo 2 ;;\n\
             *State.Running*) echo false ;;\n\
             esac\n\
             exit 0\n\
             fi\n\
             exit 1\n",
            calls = calls_path.display(),
            absent = absent_path.display(),
        );
        for binary in ["docker", "podman", "container"] {
            let path = bin.join(binary);
            std::fs::write(&path, &script).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let _path = crate::session::test_support::path_prepended(&bin);

        type Dismiss = (&'static str, fn(&mut Instance), StartBlocked);
        let dismissals: [Dismiss; 2] = [
            ("archive", Instance::archive, StartBlocked::Archived),
            ("trash", Instance::trash, StartBlocked::Trashed),
        ];
        // (label, container present, dismissal; `None` is the control)
        let mut cases = Vec::new();
        for present in [true, false] {
            cases.push((present, None));
            cases.extend(dismissals.iter().map(|d| (present, Some(d))));
        }
        for (present, dismissal) in cases {
            let label = format!(
                "{}-{}",
                if present { "stopped" } else { "create" },
                dismissal.map_or("control", |(name, ..)| name)
            );
            let _ = std::fs::remove_file(&calls_path);
            if present {
                let _ = std::fs::remove_file(&absent_path);
            } else {
                std::fs::write(&absent_path, "").unwrap();
            }
            let ready = temp.path().join(format!("{label}-ready"));
            let release = temp.path().join(format!("{label}-release"));
            let hook = format!(
                ": > {}; while [ ! -e {} ]; do sleep 0.01; done",
                crate::session::environment::shell_escape(&ready.to_string_lossy()),
                crate::session::environment::shell_escape(&release.to_string_lossy()),
            );
            crate::session::config::update_config(|global| {
                global.host_hooks.before_start = vec![hook];
            })
            .unwrap();

            let profile = format!("admit-{label}");
            let _registry = crate::tmux::status_rules::ProfileRegistryGuard::take(&profile);
            let storage = crate::session::storage::Storage::new_unwatched(&profile).unwrap();
            let mut instance = Instance::new(&label, project.to_str().unwrap());
            instance.tool = "opencode".to_string();
            instance.source_profile = profile.clone();
            instance.sandbox_info = Some(SandboxInfo {
                provider: None,
                enabled: true,
                container_id: None,
                image: "test:latest".to_string(),
                container_name: "admit".to_string(),
                extra_env: None,
                custom_instruction: None,
                before_start_env: Vec::new(),
                container_workdir: None,
            });
            let id = instance.id.clone();
            storage
                .update(|instances, _groups| {
                    instances.push(instance.clone());
                    Ok(())
                })
                .unwrap();

            let launch = std::thread::spawn(move || instance.get_container_admitted().map(|_| ()));
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !ready.exists() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            assert!(ready.exists(), "{label}: the hook never ran");
            if let Some((_, dismiss, _)) = dismissal {
                let _lock = storage.acquire_instance_lifecycle_lock(&id).unwrap();
                storage
                    .update(|instances, _groups| {
                        dismiss(instances.iter_mut().find(|row| row.id == id).unwrap());
                        Ok(())
                    })
                    .unwrap();
            }
            std::fs::write(&release, "").unwrap();
            let error = launch
                .join()
                .unwrap()
                .expect_err("the fake runtime fails every bring-up");

            let calls = std::fs::read_to_string(&calls_path).unwrap_or_default();
            let brought_up = calls.lines().any(|line| {
                ["start ", "run ", "create "]
                    .iter()
                    .any(|v| line.starts_with(v))
            });
            let case = format!("{label}: {error:#}\n{calls}");
            match dismissal {
                Some((_, _, blocked)) => {
                    assert!(!brought_up, "{case}");
                    assert_eq!(
                        error.downcast_ref::<StartBlocked>(),
                        Some(blocked),
                        "{case}"
                    );
                    let row = storage
                        .load()
                        .unwrap()
                        .into_iter()
                        .find(|row| row.id == id)
                        .unwrap();
                    assert_eq!(row.ensure_startable().unwrap_err(), *blocked, "{case}");
                }
                None => assert!(brought_up, "{case}"),
            }
        }
    }
}
