//! Acquiring the agent session id a launch resumes from.

use super::*;

const SESSION_SIDECAR_MAX_BYTES: usize = 4096;
const PRIME_AGENT_HEADER_MAX_BYTES: u64 = 64 * 1024;
const PRIME_AGENT_SETTINGS_MAX_BYTES: usize = 64 * 1024;
const PRIME_AGENT_HOME: &str = "/root/.prime/agent";
const PRIME_AGENT_DEFAULT_SESSION_DIR: &str = "/root/.prime/agent/sessions";

#[derive(Default)]
struct PrimeAgentLaunchOptions {
    cwd: Option<String>,
    session_dir: Option<String>,
    mode: Option<String>,
    no_session: bool,
}

fn parse_prime_agent_launch_options(words: &[String]) -> Option<PrimeAgentLaunchOptions> {
    const VALUE_OPTIONS: &[&str] = &[
        "--autonomous-gate",
        "--autonomous-gate-retries",
        "--autonomous-gate-timeout-ms",
        "--autonomous-max-continuations",
        "--autonomous-max-turns",
        "--autonomous-max-tokens",
        "--autonomous-timeout-ms",
        "--daemon-socket",
        "--provider",
        "--model",
        "--api-key",
        "--system-prompt",
        "--append-system-prompt",
        "--fork",
        "--models",
        "--tools",
        "-t",
        "--thinking",
        "--extension",
        "-e",
        "--skill",
        "--prompt-template",
        "--theme",
        "--goal",
        "--goal-token-budget",
    ];

    let mut options = PrimeAgentLaunchOptions::default();
    let mut index = 1;
    while let Some(argument) = words.get(index).map(String::as_str) {
        match argument {
            "--" => break,
            "--no-session" => options.no_session = true,
            "--mode" => {
                let mode = words.get(index + 1)?;
                if matches!(mode.as_str(), "text" | "json" | "rpc" | "acp" | "daemon") {
                    options.mode = Some(mode.clone());
                }
                index += 1;
            }
            "--cwd" | "--session-dir" => {
                let value = words.get(index + 1)?.clone();
                if argument == "--cwd" {
                    options.cwd = Some(value);
                } else {
                    options.session_dir = Some(value);
                }
                index += 1;
            }
            _ if VALUE_OPTIONS.contains(&argument) => {
                words.get(index + 1)?;
                index += 1;
            }
            _ => {}
        }
        index += 1;
    }
    Some(options)
}

fn resolve_prime_agent_path(value: &str, cwd: &Path) -> PathBuf {
    let expanded = if value == "~" {
        PathBuf::from("/root")
    } else if let Some(relative) = value.strip_prefix("~/") {
        Path::new("/root").join(relative)
    } else {
        PathBuf::from(value)
    };
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    };
    crate::git::template::lexical_normalize(&absolute)
}

fn read_prime_agent_settings(
    path: &Path,
) -> anyhow::Result<Option<serde_json::Map<String, serde_json::Value>>> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Prime Agent settings path has no parent"))?;
    let leaf = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("Prime Agent settings path has no file name"))?;
    let root = crate::session::AnchoredDir::open(parent)?;
    let Some(bytes) = root.read_regular(Path::new(leaf), PRIME_AGENT_SETTINGS_MAX_BYTES)? else {
        anyhow::bail!("Prime Agent settings are not a bounded regular file");
    };
    Ok(serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|settings| settings.as_object().cloned()))
}

fn read_sandbox_sidecar_file(store: &Path, instance_id: &str, leaf: &str) -> Option<Vec<u8>> {
    crate::session::validate_instance_id(instance_id).ok()?;
    let root = crate::session::AnchoredDir::open(store).ok()?;
    let relative = Path::new("aoe-session").join(instance_id).join(leaf);
    root.read_regular(&relative, SESSION_SIDECAR_MAX_BYTES)
        .ok()?
}

fn validated_prime_root_publication(
    plan: &PrimeAgentCapturePlan,
    instance_id: &str,
) -> Option<String> {
    use std::io::{BufRead as _, Read as _};

    let marker = read_sandbox_sidecar_file(&plan.store, instance_id, "root_only")?;
    if std::str::from_utf8(&marker).ok()?.trim() != "1" {
        return None;
    }
    let id = read_sandbox_sidecar_file(&plan.store, instance_id, "session_id")?;
    let id = std::str::from_utf8(&id).ok()?.trim();
    let id = crate::session::capture::validated_session_id(id.to_string())?;
    let path = read_sandbox_sidecar_file(&plan.store, instance_id, "session_path")?;
    let path = Path::new(std::str::from_utf8(&path).ok()?.trim());
    if !path.is_absolute()
        || crate::git::template::lexical_normalize(path) != path
        || path.parent()? != plan.container_session_dir
        || path.extension()?.to_str()? != "jsonl"
    {
        return None;
    }
    let file_name = path.file_name()?;
    let root = crate::session::AnchoredDir::open(&plan.store).ok()?;
    let relative = plan.session_dir.join(file_name);
    let file = root.open_regular(&relative, usize::MAX).ok()??;
    let mut header = Vec::with_capacity(4096);
    let read = std::io::BufReader::new(file)
        .take(PRIME_AGENT_HEADER_MAX_BYTES.saturating_add(1))
        .read_until(b'\n', &mut header)
        .ok()?;
    if read == 0 || u64::try_from(read).ok()? > PRIME_AGENT_HEADER_MAX_BYTES {
        return None;
    }
    let header: serde_json::Value = serde_json::from_slice(&header).ok()?;
    let valid = header.get("type").and_then(|value| value.as_str()) == Some("session")
        && header.get("id").and_then(|value| value.as_str()) == Some(id.as_str())
        && header.get("rlmDepth").and_then(|value| value.as_u64()) == Some(0)
        && header
            .get("cwd")
            .and_then(|value| value.as_str())
            .is_some_and(|cwd| {
                crate::session::capture::canonicalize_or_raw(cwd)
                    == crate::session::capture::canonicalize_or_raw(&plan.container_cwd)
            });
    valid.then_some(id)
}

impl Instance {
    pub(crate) fn prime_agent_capture_plan(&self) -> Option<PrimeAgentCapturePlan> {
        if self.resolved_capture_backend() != Some(crate::agents::SessionCaptureBackend::PrimeAgent)
            || !self.is_sandboxed()
        {
            return None;
        }
        let agent = self.resolved_agent()?;
        if !self.launch_invokes_resolved_agent_directly(agent) {
            return None;
        }
        let store = self.extension_config_bind_dir()?;
        let config = self.build_container_config().ok()?;
        self.prime_agent_capture_plan_with(&config, store)
    }

    fn prime_agent_capture_plan_with(
        &self,
        config: &crate::containers::ContainerConfig,
        store: PathBuf,
    ) -> Option<PrimeAgentCapturePlan> {
        if !config.uses_default_container_home() {
            return None;
        }

        let parsed = super::launch_command::parse_launch_command(self.get_tool_command())?;
        let mut words = parsed.words;
        words.extend(shell_words::split(&self.extra_args).ok()?);
        let options = parse_prime_agent_launch_options(&words)?;
        if options.no_session || options.mode.as_deref() == Some("daemon") {
            return None;
        }

        let launch_cwd = PathBuf::from(self.container_workdir());
        let container_cwd = options.cwd.as_deref().map_or_else(
            || launch_cwd.clone(),
            |cwd| resolve_prime_agent_path(cwd, &launch_cwd),
        );
        if !container_cwd.is_absolute() {
            return None;
        }

        let environment_value = |key: &str| {
            config
                .environment
                .iter()
                .find(|entry| entry.key() == key)
                .map(|entry| entry.value())
        };
        let environment_session_dir = environment_value("PRIME_AGENT_SESSION_DIR")
            .or_else(|| environment_value("PRIME_AGENT_CODING_AGENT_SESSION_DIR"))
            .filter(|value| !value.is_empty());
        let configured_session_dir = options
            .session_dir
            .filter(|value| !value.is_empty())
            .or_else(|| environment_session_dir.map(str::to_string));

        let session_dir_value = if let Some(value) = configured_session_dir {
            value
        } else {
            let global_container_path = Path::new(PRIME_AGENT_HOME).join("settings.json");
            let project_container_path = container_cwd.join(".prime/agent/settings.json");
            let global_host_path =
                config.host_path_for_container_path(&global_container_path, false)?;
            let project_host_path =
                config.host_path_for_container_path(&project_container_path, false)?;
            let global = read_prime_agent_settings(&global_host_path).ok()?;
            let project = read_prime_agent_settings(&project_host_path).ok()?;
            match project
                .as_ref()
                .and_then(|settings| settings.get("sessionDir"))
                .or_else(|| {
                    global
                        .as_ref()
                        .and_then(|settings| settings.get("sessionDir"))
                }) {
                Some(serde_json::Value::String(value)) => value.clone(),
                Some(serde_json::Value::Null) | None => PRIME_AGENT_DEFAULT_SESSION_DIR.to_string(),
                Some(_) => return None,
            }
        };

        let container_session_dir = resolve_prime_agent_path(&session_dir_value, &container_cwd);
        let session_dir = container_session_dir
            .strip_prefix(Path::new(PRIME_AGENT_HOME))
            .ok()?
            .to_path_buf();
        if config.host_path_for_container_path(&container_session_dir, true)?
            != store.join(&session_dir)
        {
            return None;
        }
        Some(PrimeAgentCapturePlan {
            store,
            session_dir,
            container_session_dir,
            container_cwd: container_cwd.to_str()?.to_string(),
        })
    }
    /// Acquire a pre-launch session ID for the agent.
    ///
    /// Returns `(session_id, is_existing)`. Explicit use, clear, and fork
    /// intents win. Default intent keeps a stored id, but replaces it when the
    /// pane-scoped backend proves the running pane rotated to another native
    /// conversation. With no stored id, capture is attempted only while the
    /// pane exists and only through the backend/context declared by the agent.
    /// A miss starts fresh; no shared store is searched without its launch
    /// floor and ownership contract.
    pub fn acquire_session_id(&mut self) -> (Option<String>, bool) {
        // Both pre-mint decisions are made here rather than inside
        // acquire_session_id_with: it keeps the config read and the binary
        // probe off every other launch, and keeps the inner fn a pure,
        // testable seam.
        let backend = self.resolved_capture_backend();
        let preassign = backend == Some(crate::agents::SessionCaptureBackend::OpenCode)
            && self.opencode_preassign_enabled();
        let pin_pi = self.pi_session_id_pinnable();
        let preassign_environment = preassign.then(|| self.resolved_host_environment());
        self.acquire_session_id_with(&|path| {
            if pin_pi {
                return Some(crate::session::capture::generate_session_uuid());
            }
            preassign_environment.as_deref().and_then(|environment| {
                crate::session::capture::preassign_opencode_session_id(path, environment)
            })
        })
    }

    /// Session-id acquisition with the pre-mint step injected as a seam, so
    /// tests can drive the fresh-launch arms without a real opencode binary,
    /// network, or installed pi. Production wraps this with the live preassign
    /// helper and the Pi pin.
    fn acquire_session_id_with(
        &mut self,
        mint_fresh_id: &dyn Fn(&str) -> Option<String>,
    ) -> (Option<String>, bool) {
        match self.resume_intent.clone() {
            ResumeIntent::Use(sid) => {
                self.agent_session_id = Some(sid.clone());
                return (Some(sid), true);
            }
            ResumeIntent::Cleared => {
                self.agent_session_id = None;
                self.resume_probe_failed_sid = None;
                // The transcript belonged to the conversation being dropped.
                // `pi_resumable_transcript` would refuse it on the id check
                // anyway; not carrying it is one less thing depending on that.
                self.pi_session_path = None;
                let session_id = self.fresh_launch_session_id(mint_fresh_id);
                if let Some(ref id) = session_id {
                    self.agent_session_id = Some(id.clone());
                }
                return (session_id, false);
            }
            ResumeIntent::Fork { .. } => {
                // The child id was pre-generated and stored in
                // agent_session_id at creation. acquire returns it as the
                // session this instance owns; the actual fork flags
                // (--resume <parent> --fork-session --session-id <child>) are
                // emitted by apply_session_flags, which reads the parent off
                // the Fork intent. Report `false` (not an in-place resume): a
                // fork starts a new session.
                return (self.agent_session_id.clone(), false);
            }
            ResumeIntent::Default => {}
        }

        if let Some(stored) = self.agent_session_id.clone() {
            // Rebinding rather than returning early runs the observation
            // through the same empty-thread downgrade as the stored id below.
            // The SessionStart hook fires before Claude writes any content, so
            // the sidecar can legitimately name a thread with no transcript.
            let stored = match self.capture_freshest_session_id() {
                Some(fresh) => {
                    tracing::info!(
                        target: "session.store",
                        stale = %stored,
                        fresh = %fresh,
                        tool = %self.tool,
                        "Replacing stored session id with fresher live observation"
                    );
                    self.agent_session_id = Some(fresh.clone());
                    fresh
                }
                None => stored,
            };
            // A stored Claude sid with no transcript on disk is not resumable:
            // Claude minted the UUID at first launch but nothing was ever
            // written (an empty thread killed before the first prompt), so
            // `--resume <sid>` is a guaranteed launch failure that lands the
            // session in the "resume failed for sid ...; preserved for explicit
            // retry" state. Launch it as a fresh pinned session instead
            // (`is_existing = false` -> `--session-id <sid>`), which succeeds
            // and keeps the id stable so a later first prompt stays continuous.
            // Pi is pre-minted too, but it needs no equivalent branch: its
            // pin flag is also its create flag, so `apply_session_flags`
            // relaunches an unwritten pin with `--session-id` and pi recreates
            // the conversation under the same id (see
            // `resume_flag_arm_is_existing`). Host-only: a sandboxed transcript
            // lives inside the container, which may not be up at acquire time.
            if self.resolved_capture_backend() == Some(crate::agents::SessionCaptureBackend::Claude)
                && !self.is_sandboxed()
                && crate::session::capture::claude_host_transcript_confirmed_absent(
                    &self.project_path,
                    &stored,
                    &self.resolved_host_environment(),
                )
            {
                tracing::info!(
                    target: "session.store",
                    sid = %stored,
                    "stored Claude sid has no transcript on disk; launching fresh \
                     with --session-id instead of --resume to avoid a certain \
                     resume failure"
                );
                return (Some(stored), false);
            }
            return (Some(stored), true);
        }

        let tmux_exists = self.tmux_session().is_ok_and(|s| s.exists());
        if tmux_exists {
            if let Some(id) = self.try_retroactive_capture() {
                tracing::info!(target: "session.store",
                    "Retroactive capture found session ID for {}: {}",
                    self.tool,
                    id
                );
                self.agent_session_id = Some(id);
                return (self.agent_session_id.clone(), true);
            }
        }

        let session_id = self.fresh_launch_session_id(mint_fresh_id);

        if let Some(ref id) = session_id {
            tracing::debug!(target: "session.store", "Session ID for {}: {}", self.tool, id);
            self.agent_session_id = session_id.clone();
        }

        (session_id, false)
    }

    /// Mint the session id for a brand-new launch. Claude and eligible Pi
    /// launches pin a UUID. Direct host OpenCode launches pre-create a session
    /// automatically. A preassign failure returns no id rather than guessing
    /// from the shared store. Other supported backends capture after launch.
    fn fresh_launch_session_id(
        &self,
        mint_fresh_id: &dyn Fn(&str) -> Option<String>,
    ) -> Option<String> {
        match self.resolved_capture_backend()? {
            crate::agents::SessionCaptureBackend::Claude => Some(generate_session_uuid()),
            crate::agents::SessionCaptureBackend::OpenCode
            | crate::agents::SessionCaptureBackend::Pi => mint_fresh_id(&self.project_path),
            _ => None,
        }
    }

    /// Whether automatic OpenCode session-id preassignment applies. It is an
    /// opt-in host-only operation and requires a direct OpenCode launch.
    fn opencode_preassign_enabled(&self) -> bool {
        if self.is_sandboxed()
            || !crate::session::config::profile_config::resolve_config_or_warn(
                &self.effective_profile(),
            )
            .session
            .opencode_preassign_session_id
        {
            return false;
        }
        self.opencode_launch_mirrorable_by_ambient_serve()
    }

    /// Whether the ephemeral `opencode serve` used for preassignment runs the
    /// same binary as the real launch. The caller passes the resolved launch
    /// environment to both processes so their data-store routing also matches.
    fn opencode_launch_mirrorable_by_ambient_serve(&self) -> bool {
        self.resolved_agent().is_some_and(|agent| {
            agent.name == "opencode" && self.launch_invokes_resolved_agent_directly(agent)
        })
    }

    /// Best-effort backfill of a missing `agent_session_id` during a read-only
    /// CLI command.
    ///
    /// Eligibility requires default intent, an active live pane, no competing
    /// id-less session for the same tool and cwd, and lifecycle ownership. The
    /// declared capture context remains authoritative: pane-scoped sources may
    /// publish their own id, while managed stores still require the in-memory
    /// launch floor and exclusive lease. A miss or CAS race is a silent no-op.
    fn self_heal_row_is_eligible(&self, contended: &HashSet<(String, String)>) -> bool {
        self.agent_session_id.is_none()
            && self.resume_intent.is_default()
            && !matches!(self.status, Status::Deleting | Status::Creating)
            && self.effective_bucket() == SessionBucket::Active
            && !contended.contains(&self.contended_capture_key())
    }

    pub(crate) fn self_heal_session_id(
        &mut self,
        profile: &str,
        contended: &HashSet<(String, String)>,
    ) {
        if !self.self_heal_row_is_eligible(contended) {
            return;
        }
        if !self.tmux_alive_cached() {
            return;
        }
        let file_watch = self.resolve_file_watch();
        let ownership: Result<_> = (|| {
            let storage = crate::session::storage::Storage::new(profile, file_watch.clone())?;
            let lifecycle_lock = storage.acquire_instance_lifecycle_lock(&self.id)?;
            let generation = storage.update(|instances, _groups| {
                let Some(stored) = instances.iter_mut().find(|instance| instance.id == self.id)
                else {
                    anyhow::bail!("session disappeared before capture");
                };
                if stored.agent_session_id.is_some()
                    || !stored.resume_intent.is_default()
                    || matches!(stored.status, Status::Deleting | Status::Creating)
                    || stored.effective_bucket() != SessionBucket::Active
                {
                    anyhow::bail!("session is no longer eligible for capture");
                }
                stored
                    .try_acquire_lifecycle_reservation(
                        LifecycleOperation::Capture,
                        Self::LIFECYCLE_RESERVATION_TTL,
                        Utc::now(),
                    )
                    .map_err(|error| anyhow::anyhow!("capture blocked: {error}"))
            })?;
            Ok((storage, lifecycle_lock, generation))
        })();
        let Ok((storage, _lifecycle_lock, generation)) = ownership else {
            return;
        };
        let captured = self.try_retroactive_capture();
        let applied = captured.as_ref().is_some_and(|captured| {
            self.resume_probe_failed_sid.as_deref() != Some(captured.as_str())
                && persist_session_to_storage(profile, &self.id, captured, None, &file_watch)
                    == SidWrite::Applied
        });
        let released = storage.update(|instances, _groups| {
            let Some(stored) = instances.iter_mut().find(|instance| instance.id == self.id) else {
                return Ok(false);
            };
            Ok(stored
                .release_lifecycle_reservation_if_owned(LifecycleOperation::Capture, generation))
        });
        if !matches!(released, Ok(true)) {
            tracing::warn!(
                target: "session.sync",
                instance = %self.id,
                "self-heal capture lost its lifecycle reservation before release",
            );
            return;
        }
        self.lifecycle_generation = generation;
        self.lifecycle_reservation = None;
        if applied {
            self.agent_session_id = captured;
            self.resume_probe_failed_sid = None;
            tracing::info!(
                target: "session.store",
                instance = %self.id,
                tool = %self.tool,
                "backfilled agent_session_id from a read-only CLI command; \
                 resume is now available without a TUI or daemon",
            );
        }
    }

    /// Return a newly observed native id only when the declared backend can
    /// attribute it to this running pane. Claude, Cursor, Qwen, and Kiro read
    /// their instance-keyed hook sidecar; Pi reads its instance-keyed extension
    /// sidecar. Managed store backends route through their exact store, cwd,
    /// launch-floor, exclusion, and lease checks. No backend falls through to a
    /// different identity source.
    pub(crate) fn capture_freshest_session_id(&self) -> Option<String> {
        let backend = self.resolved_capture_backend()?;
        if backend == crate::agents::SessionCaptureBackend::Pi {
            let authoritative = self.pi_published_session_id(false)?;
            if self.retroactive_capture_excludes.contains(&authoritative) {
                return None;
            }
            return override_if_distinct(self.agent_session_id.as_deref(), authoritative);
        }
        if backend == crate::agents::SessionCaptureBackend::PrimeAgent {
            let authoritative = self.prime_published_root_session_id()?;
            if self.retroactive_capture_excludes.contains(&authoritative) {
                return None;
            }
            return override_if_distinct(self.agent_session_id.as_deref(), authoritative);
        }
        if matches!(
            backend,
            crate::agents::SessionCaptureBackend::Claude
                | crate::agents::SessionCaptureBackend::HookSidecar
        ) {
            let authoritative = crate::hooks::read_hook_session_id_any_age(&self.id)?;
            if self.retroactive_capture_excludes.contains(&authoritative) {
                return None;
            }
            return override_if_distinct(self.agent_session_id.as_deref(), authoritative);
        }
        let live = self.try_retroactive_capture()?;
        override_if_distinct(self.agent_session_id.as_deref(), live)
    }

    #[cfg(test)]
    pub(crate) fn mark_pi_extension_launched_for_test(&mut self) {
        self.pi_extension_launched = true;
    }

    /// Extension flag and sidecar environment for an identity-publishing backend.
    pub(super) fn identity_extension_launch(&self) -> Option<(String, String)> {
        let backend = self.resolved_capture_backend()?;
        backend.identity_publisher()?;
        if self.is_sandboxed() {
            let bind_dir = self.extension_config_bind_dir()?;
            let config = self.build_container_config().ok()?;
            let (container_root, flag) = match backend {
                crate::agents::SessionCaptureBackend::Pi => {
                    crate::session::config::container_config::install_pi_sandbox_extension_at(
                        &bind_dir,
                    )
                    .ok()?;
                    (Path::new("/root/.pi"), String::new())
                }
                crate::agents::SessionCaptureBackend::PrimeAgent => {
                    self.prime_agent_capture_plan_with(&config, bind_dir.clone())?;
                    crate::session::config::container_config::install_prime_sandbox_extension_at(
                        &bind_dir,
                    )
                    .ok()?;
                    (
                        Path::new(
                            crate::session::config::container_config::PRIME_AGENT_DIR_IN_CONTAINER,
                        ),
                        format!(
                            " -e {}",
                            shell_escape(
                                crate::session::config::container_config::PRIME_AGENT_EXTENSION_IN_CONTAINER
                            )
                        ),
                    )
                }
                _ => return None,
            };
            if !config.uses_default_container_home()
                || !config.path_is_mounted(&bind_dir, container_root, true)
            {
                return None;
            }
            let container = crate::containers::DockerContainer::from_session_id(&self.id);
            let container_known = self
                .sandbox_info
                .as_ref()
                .and_then(|sandbox| sandbox.container_id.as_ref())
                .is_some()
                || container.exists().ok() == Some(true);
            if container_known && container.mount_fingerprint_matches(&config).ok()? != Some(true) {
                return None;
            }
            let sidecar_root = match backend {
                crate::agents::SessionCaptureBackend::Pi => {
                    crate::session::config::container_config::PI_SIDECAR_DIR_IN_CONTAINER
                }
                crate::agents::SessionCaptureBackend::PrimeAgent => {
                    "/root/.prime/agent/aoe-session"
                }
                _ => return None,
            };
            return Some((
                flag,
                format!(
                    "AOE_SESSION_ID_FILE={}/{}/session_id ",
                    sidecar_root, self.id
                ),
            ));
        }
        if backend != crate::agents::SessionCaptureBackend::Pi
            || super::launch_command::environment_defines_path(&self.resolved_host_environment())
            || !crate::agents::pi_supports_extension_flag()
        {
            return None;
        }
        let extension = super::launch_command::session_identity_extension_path().ok()?;
        let sidecar = crate::hooks::ensure_instance_dir_path(&self.id)
            .ok()?
            .join("session_id");
        Some((
            format!(" -e {}", shell_escape(&extension.to_string_lossy())),
            format!(
                "AOE_SESSION_ID_FILE={} ",
                shell_escape(&sidecar.to_string_lossy())
            ),
        ))
    }

    /// Whether this Pi pane publishes its conversation through the AoE
    /// extension, which is what makes its observations name a pane.
    ///
    /// Read from what the launch did, not from the binary probe: an upgrade
    /// mid-session must not reclassify a pane that is already running.
    /// The conversation this pane published, whichever side of the container
    /// boundary it published on. `any_age` drops the freshness window, which a
    /// final flush wants and a resume does not.
    pub(crate) fn pi_published_session_id(&self, any_age: bool) -> Option<String> {
        if self.resolved_capture_backend() != Some(crate::agents::SessionCaptureBackend::Pi) {
            return None;
        }
        match self.extension_sidecar_source()? {
            SessionSidecarSource::HostHooks => {
                if any_age {
                    crate::hooks::read_hook_session_id_any_age(&self.id)
                } else {
                    crate::hooks::read_hook_session_id(&self.id)
                }
            }
            SessionSidecarSource::SandboxDir(_) => {
                let raw = self.read_extension_sandbox_file("session_id")?;
                let id = std::str::from_utf8(&raw).ok()?.trim();
                uuid::Uuid::parse_str(id).ok().map(|_| id.to_string())
            }
        }
    }

    /// The transcript path this pane published, as the pane sees it. In a
    /// container that is a /root/.pi/ path, which is what pi's argv needs;
    /// pi_host_view_of maps it back for host-side checks.
    pub(crate) fn pi_published_session_path(&self) -> Option<String> {
        if self.resolved_capture_backend() != Some(crate::agents::SessionCaptureBackend::Pi) {
            return None;
        }
        self.published_extension_session_path()
    }

    fn published_extension_session_path(&self) -> Option<String> {
        match self.extension_sidecar_source()? {
            SessionSidecarSource::HostHooks => crate::hooks::read_hook_session_path(&self.id),
            SessionSidecarSource::SandboxDir(_) => {
                let raw = self.read_extension_sandbox_file("session_path")?;
                let path = std::str::from_utf8(&raw).ok()?.trim();
                path.starts_with('/').then(|| path.to_string())
            }
        }
    }

    /// Where this Pi pane publishes, or None when that cannot be established.
    pub(crate) fn pi_sidecar_source(&self) -> Option<SessionSidecarSource> {
        (self.resolved_capture_backend() == Some(crate::agents::SessionCaptureBackend::Pi))
            .then(|| self.extension_sidecar_source())?
    }

    fn extension_sidecar_source(&self) -> Option<SessionSidecarSource> {
        self.resolved_capture_backend()?.identity_publisher()?;
        if self.is_sandboxed() {
            crate::session::validate_instance_id(&self.id).ok()?;
            return Some(SessionSidecarSource::SandboxDir(
                self.extension_config_bind_dir()?
                    .join("aoe-session")
                    .join(&self.id),
            ));
        }
        Some(SessionSidecarSource::HostHooks)
    }

    fn extension_config_bind_dir(&self) -> Option<std::path::PathBuf> {
        self.sandbox_capture_store_dir()
    }

    fn read_extension_sandbox_file(&self, leaf: &str) -> Option<Vec<u8>> {
        read_sandbox_sidecar_file(&self.extension_config_bind_dir()?, &self.id, leaf)
    }

    fn extension_sandbox_regular_exists(&self, relative: &Path) -> bool {
        self.extension_config_bind_dir()
            .and_then(|root| crate::session::AnchoredDir::open(&root).ok())
            .is_some_and(|root| root.regular_exists(relative))
    }

    /// A published Pi path as the host filesystem sees it.
    fn pi_host_view_of(&self, published: &str) -> Option<std::path::PathBuf> {
        if !self.is_sandboxed() {
            return Some(std::path::PathBuf::from(published));
        }
        let rest = published.strip_prefix("/root/.pi/")?;
        Some(self.extension_config_bind_dir()?.join(rest))
    }

    fn pi_resumable_transcript(&self) -> Option<String> {
        let path = self.pi_session_path.as_deref()?;
        let id = self.agent_session_id.as_deref()?;
        let name = std::path::Path::new(path).file_name()?.to_str()?;
        let names_this_conversation = name
            .rsplit_once('_')
            .and_then(|(_, tail)| tail.strip_suffix(".jsonl"))
            .is_some_and(|uuid| uuid == id);
        let exists = if self.is_sandboxed() {
            path.strip_prefix("/root/.pi/")
                .is_some_and(|relative| self.extension_sandbox_regular_exists(Path::new(relative)))
        } else {
            self.pi_host_view_of(path)
                .is_some_and(|host_path| host_path.is_file())
        };
        (names_this_conversation && exists).then(|| path.to_string())
    }

    pub(super) fn absorb_published_pi_session(&mut self) {
        if self.resolved_capture_backend() != Some(crate::agents::SessionCaptureBackend::Pi) {
            return;
        }
        if let Some(path) = self.pi_published_session_path() {
            self.pi_session_path = Some(path);
        }
    }

    fn prime_published_root_session_id(&self) -> Option<String> {
        let plan = self.prime_agent_capture_plan()?;
        validated_prime_root_publication(&plan, &self.id)
    }

    pub(super) fn absorb_published_prime_session(&mut self) -> bool {
        let Some(id) = self.prime_published_root_session_id() else {
            return false;
        };
        if self.agent_session_id.as_deref() == Some(id.as_str()) {
            return false;
        }
        self.agent_session_id = Some(id);
        true
    }

    pub(crate) fn prime_root_sidecar_poll_fn(
        &self,
        plan: PrimeAgentCapturePlan,
    ) -> Box<dyn Fn() -> Option<String> + Send + 'static> {
        let instance_id = self.id.clone();
        Box::new(move || validated_prime_root_publication(&plan, &instance_id))
    }

    pub(crate) fn uses_pi_session_sidecar(&self) -> bool {
        self.resolved_capture_backend() == Some(crate::agents::SessionCaptureBackend::Pi)
            && self.pi_sidecar_source().is_some()
            && (self.pi_extension_launched || self.extension_sidecar_exists())
    }

    fn extension_sidecar_exists(&self) -> bool {
        match self.extension_sidecar_source() {
            Some(SessionSidecarSource::SandboxDir(_)) => {
                let relative = Path::new("aoe-session").join(&self.id).join("session_id");
                self.extension_sandbox_regular_exists(&relative)
            }
            Some(SessionSidecarSource::HostHooks) => {
                crate::hooks::session_id_sidecar_exists(&self.id)
            }
            None => false,
        }
    }

    pub(super) fn clear_pane_identity_sidecar(&self) {
        match self.resolved_capture_backend() {
            Some(
                crate::agents::SessionCaptureBackend::Claude
                | crate::agents::SessionCaptureBackend::HookSidecar,
            ) => {
                let _ = crate::hooks::unlink_session_id_via_guard(&self.id);
            }
            Some(
                crate::agents::SessionCaptureBackend::Pi
                | crate::agents::SessionCaptureBackend::PrimeAgent,
            ) => match self.extension_sidecar_source() {
                Some(SessionSidecarSource::HostHooks) => {
                    let _ = crate::hooks::unlink_session_id_via_guard(&self.id);
                }
                Some(SessionSidecarSource::SandboxDir(_)) => {
                    if let Some(root_path) = self.extension_config_bind_dir() {
                        if let Ok(root) = crate::session::AnchoredDir::open(&root_path) {
                            let base = Path::new("aoe-session").join(&self.id);
                            let _ = root.remove_file(&base.join("session_id"));
                            let _ = root.remove_file(&base.join("session_path"));
                            let _ = root.remove_file(&base.join("root_only"));
                        }
                    }
                }
                None => {}
            },
            _ => {}
        }
    }

    /// Whether this session may pin its Pi conversation with `--session-id`.
    ///
    /// Requires a directly verified host Pi launch with an unmodified PATH.
    fn pi_session_id_pinnable(&self) -> bool {
        self.resolved_capture_backend() == Some(crate::agents::SessionCaptureBackend::Pi)
            && !self.is_sandboxed()
            && !super::launch_command::environment_defines_path(&self.resolved_host_environment())
            && crate::agents::pi_supports_session_id_flag()
    }

    /// Whether a launch emits the `existing` arm of the agent's
    /// [`ResumeStrategy`].
    ///
    /// It tracks `is_existing` except for Pi on a pinnable binary, where the
    /// pinning arm serves both: pi writes its session file on the first
    /// message, so a pane pinned and never prompted holds an id `--session`
    /// exits 1 on, and `--session-id` recreates it.
    fn resume_flag_arm_is_existing(
        &self,
        is_existing: bool,
        pi_pinnable: bool,
        session_id: Option<&str>,
        explicitly_pinned: bool,
    ) -> bool {
        // `--session-id` searches this project only and creates the
        // conversation when it is absent, so it is for ids AoE minted. A value
        // the user pinned keeps `--session`, which resolves partials and
        // searches wider; its shape says nothing about its origin.
        let takes_pinning_arm = self.resolved_capture_backend()
            == Some(crate::agents::SessionCaptureBackend::Pi)
            && pi_pinnable
            && !explicitly_pinned
            && session_id.is_some_and(|sid| uuid::Uuid::parse_str(sid).is_ok());
        is_existing && !takes_pinning_arm
    }

    /// Why this row can never resume, decided from the agent registry and the
    /// launch shape alone. Reports what `apply_session_flags` below actually
    /// refuses, so the availability the API and TUI render cannot drift from
    /// the launch line.
    fn terminal_resume_static_unavailable(&self) -> Option<ResumeStaticUnavailable> {
        let Some(agent) = self.resolved_agent() else {
            return Some(ResumeStaticUnavailable::Agent);
        };
        if agent.session_support.is_none() {
            return Some(ResumeStaticUnavailable::Agent);
        }
        // A launcher, a path-qualified script, or shell syntax between the
        // pane and the agent means no selector reaches the agent's own argv.
        if !self.launch_can_carry_resume_selector(agent) {
            return Some(ResumeStaticUnavailable::Command);
        }
        // Copilot publishes no identity in any environment, so a sandbox has
        // nothing of its own to resume. An explicit pin still names a
        // conversation and is attempted against this instance's own store.
        if agent.name == "copilot"
            && self.is_sandboxed()
            && !matches!(self.resume_intent, ResumeIntent::Use(_))
        {
            return Some(ResumeStaticUnavailable::Sandbox);
        }
        None
    }

    fn terminal_resume_explicit_target_invalid(&self) -> bool {
        matches!(
            &self.resume_intent,
            ResumeIntent::Use(target) if !is_valid_session_id(target)
        )
    }

    pub(crate) fn terminal_context_resume_cached(&self) -> TerminalContextResume {
        self.terminal_context_resume_with_runtime_source(|| {
            self.tmux_session()
                .map(|session| crate::tmux::cached_session_existence(session.name()))
                .unwrap_or(crate::tmux::SessionExistence::Unknown)
        })
    }

    fn terminal_context_resume_with_runtime_source(
        &self,
        runtime_source: impl FnOnce() -> crate::tmux::SessionExistence,
    ) -> TerminalContextResume {
        if let Some(unavailable) = self.terminal_resume_static_unavailable() {
            return match unavailable {
                ResumeStaticUnavailable::Agent => TerminalContextResume::AgentUnsupported,
                ResumeStaticUnavailable::Sandbox => TerminalContextResume::SandboxUnsupported,
                ResumeStaticUnavailable::Command => TerminalContextResume::CommandUnsupported,
            };
        }
        match &self.resume_intent {
            ResumeIntent::Cleared => TerminalContextResume::ForcedFresh,
            ResumeIntent::Fork { .. } => TerminalContextResume::ForkPending,
            ResumeIntent::Use(_) => {
                if self.terminal_resume_explicit_target_invalid() {
                    TerminalContextResume::InvalidTarget
                } else {
                    TerminalContextResume::Available
                }
            }
            ResumeIntent::Default => {
                if self.agent_session_id.is_some()
                    && self.agent_session_id == self.resume_probe_failed_sid
                {
                    TerminalContextResume::PreviousFailure
                } else if self.agent_session_id.is_some()
                    || !matches!(runtime_source(), crate::tmux::SessionExistence::Absent)
                {
                    TerminalContextResume::RuntimeCheckRequired
                } else {
                    TerminalContextResume::NoTarget
                }
            }
        }
    }

    fn existing_session_selector(&self, words: &[String]) -> Option<String> {
        let agent = self.resolved_agent()?;
        let strategy = agent.session_support.as_ref()?.resume;
        if words.first().map(String::as_str) != Some(agent.binary) {
            return None;
        }
        let flag_present = |flag: &str| {
            words.iter().skip(1).any(|word| {
                word == flag
                    || word
                        .strip_prefix(flag)
                        .is_some_and(|rest| rest.starts_with('='))
            })
        };
        if agent.name == "claude" {
            if let Some(selector) = [
                "-c",
                "--continue",
                "-r",
                "--from-pr",
                "--teleport",
                "--cloud",
                "--remote",
                "--fork-session",
            ]
            .into_iter()
            .find(|selector| flag_present(selector))
            {
                return Some(selector.to_string());
            }
        }
        match strategy {
            crate::agents::ResumeStrategy::Flag(flag) => {
                flag_present(flag).then(|| flag.to_string())
            }
            crate::agents::ResumeStrategy::FlagPair {
                existing,
                new_session,
            } => [existing, new_session]
                .into_iter()
                .find(|flag| flag_present(flag))
                .map(str::to_string),
            crate::agents::ResumeStrategy::Subcommand(subcommand) => words
                .get(1)
                .is_some_and(|word| word == subcommand)
                .then(|| subcommand.to_string()),
        }
    }

    pub(super) fn apply_session_flags(&mut self, cmd: &mut String, context: &str) -> Result<bool> {
        let Some(parsed_command) = parse_launch_command(cmd) else {
            return Ok(false);
        };
        if let Some(selector) = self.existing_session_selector(&parsed_command.words) {
            let aoe_has_state = self.agent_session_id.is_some()
                || matches!(
                    self.resume_intent,
                    ResumeIntent::Use(_) | ResumeIntent::Fork { .. }
                );
            if aoe_has_state {
                anyhow::bail!(
                    "{context} command already contains native session selector {selector}; remove it or clear the AoE-managed resume state before launching"
                );
            }
            tracing::info!(target: "session.store", %context, %selector,
                "command supplies its own native session selector; skipping AoE session injection");
            return Ok(false);
        }
        if !self.supports_native_resume() {
            return Ok(false);
        }
        if let ResumeIntent::Fork { from } = self.resume_intent.clone() {
            let child = self.agent_session_id.clone();
            if let Some(child_id) = child.as_deref() {
                let resume_tool = self
                    .resolved_agent()
                    .map_or(self.tool.as_str(), |agent| agent.name);
                let fork_part = build_fork_flags(resume_tool, &from, child_id);
                if !fork_part.is_empty() {
                    // Codex's fork is a subcommand and must sit right after the
                    // binary (before other flags), like its resume subcommand.
                    // Flag-shaped forks (claude, opencode) append.
                    let is_subcommand = matches!(
                        self.resolved_agent().map(|agent| &agent.fork_strategy),
                        Some(crate::agents::ForkStrategy::CodexFork)
                    );
                    splice_subcommand_or_append(
                        cmd,
                        &fork_part,
                        is_subcommand.then_some(parsed_command.executable_end),
                    );
                }
            }
            // A fork is a fresh session, not an in-place resume.
            return Ok(false);
        }
        // Read before acquisition: a Use intent marks an id the user pinned
        // rather than one AoE minted or captured.
        let explicitly_pinned = matches!(self.resume_intent, ResumeIntent::Use(_));
        self.absorb_published_pi_session();
        self.absorb_published_prime_session();
        let (mut session_id, is_existing) = self.acquire_session_id();
        // Which ResumeStrategy arm to emit. Pi diverges from `is_existing`
        // (see `resume_flag_arm_is_existing`), so the launch flag and the
        // "this was a resume" answer this fn returns are decided separately.
        let flag_arm_is_existing = self.resume_flag_arm_is_existing(
            is_existing,
            self.pi_session_id_pinnable(),
            session_id.as_deref(),
            explicitly_pinned,
        );
        // Only the constraints `build_resume_flags` cannot see are applied
        // here. It already refuses an unresolved agent, an agent with no
        // session support and an invalid id, and says why; suppressing the sid
        // for those would drop the launch line's only trace of the decision.
        //
        // `Sandbox` covers copilot's stale host id, which must not cross into
        // the sandbox namespace. It already excludes an explicit pin, which
        // stays authoritative against this instance's own store.
        let static_unavailable = self.terminal_resume_static_unavailable();
        if matches!(static_unavailable, Some(ResumeStaticUnavailable::Command)) {
            tracing::warn!(target: "session.store",
                tool = %self.tool,
                command = %self.command,
                "resume selectors need the agent's own argv and this command hides it behind a launcher; starting fresh"
            );
        }
        if matches!(
            static_unavailable,
            Some(ResumeStaticUnavailable::Sandbox | ResumeStaticUnavailable::Command)
        ) {
            session_id = None;
        }
        // A transcript the pane published outranks its id: `--session <path>`
        // resolves the conversation wherever it was started, while
        // `--session-id` looks only in the current project and would create an
        // empty one under the same uuid after a worktree move.
        // Never over an explicit pin: the user named a conversation, and a
        // stored path is AoE's own bookkeeping.
        if is_existing && !explicitly_pinned && session_id.is_some() {
            if let Some(path) = self.pi_resumable_transcript() {
                let flags = format!("--session {}", shell_escape(&path));
                splice_subcommand_or_append(cmd, &flags, None);
                tracing::debug!(target: "session.store", "Added resume flags to {} command: {}", context, flags);
                return Ok(true);
            }
        }
        let resume_tool = self
            .resolved_agent()
            .map_or(self.tool.as_str(), |agent| agent.name);
        let emitted = append_resume_flags(
            resume_tool,
            session_id.as_deref(),
            flag_arm_is_existing,
            cmd,
            parsed_command.executable_end,
            context,
        );
        Ok(is_existing && emitted)
    }

    /// Persist an ambiguous resume-probe failure without clearing the durable
    /// resume sid. The CAS guard keeps peer sid changes authoritative.
    pub(super) fn mark_resume_probe_failed(&mut self, profile: &str, sid: &str) -> SidWrite {
        let storage =
            match crate::session::storage::Storage::new(profile, self.resolve_file_watch()) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(target: "session.store",
                        "Failed to create storage for resume-probe failure marker for {}: {}",
                        self.id,
                        e
                    );
                    return SidWrite::Failed;
                }
            };

        let instance_id = self.id.clone();
        let sid_for_closure = sid.to_string();
        let outcome = storage.update(|instances, _groups| {
            let Some(inst) = instances.iter_mut().find(|i| i.id == instance_id) else {
                return Ok(SidWrite::Failed);
            };

            if inst.agent_session_id.as_deref() != Some(sid_for_closure.as_str()) {
                tracing::warn!(target: "session.store",
                    instance_id = %instance_id,
                    expected_sid = %sid_for_closure,
                    disk_sid = ?inst.agent_session_id,
                    "sid CAS mismatch in resume-probe failure marker; skipping write"
                );
                return Ok(SidWrite::Skipped);
            }

            inst.resume_probe_failed_sid = Some(sid_for_closure.clone());
            Ok(SidWrite::Applied)
        });

        match outcome {
            Ok(write @ (SidWrite::Applied | SidWrite::Skipped)) => {
                if let Ok(insts) = storage.load() {
                    if let Some(disk) = insts.into_iter().find(|i| i.id == self.id) {
                        self.agent_session_id = disk.agent_session_id;
                        self.resume_intent = disk.resume_intent;
                        self.resume_probe_failed_sid = disk.resume_probe_failed_sid;
                    }
                }
                write
            }
            Ok(SidWrite::Failed) => {
                tracing::warn!(target: "session.store",
                    "Resume-probe failure marker found no instance row for {}",
                    self.id
                );
                SidWrite::Failed
            }
            Err(e) => {
                tracing::warn!(target: "session.store",
                    "Failed to mark resume-probe failure for {}: {}",
                    self.id,
                    e
                );
                SidWrite::Failed
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::instance::launch_command::build_resume_flags;
    use crate::session::instance::test_helpers::*;
    use crate::session::test_support::EnvGuard;
    use serial_test::serial;

    #[test]
    fn self_heal_eligibility_rejects_owned_and_inactive_rows() {
        let base = Instance::new("self-heal", "/tmp/self-heal");
        let empty = HashSet::new();
        assert!(base.self_heal_row_is_eligible(&empty));

        let mut with_id = base.clone();
        with_id.agent_session_id = Some("native-id".to_string());
        let mut cleared = base.clone();
        cleared.resume_intent = ResumeIntent::Cleared;
        let mut deleting = base.clone();
        deleting.status = Status::Deleting;
        let mut creating = base.clone();
        creating.status = Status::Creating;
        let mut archived = base.clone();
        archived.archived_at = Some(Utc::now());

        for (reason, instance) in [
            ("stored identity", with_id),
            ("cleared intent", cleared),
            ("deleting", deleting),
            ("creating", creating),
            ("archived", archived),
        ] {
            assert!(
                !instance.self_heal_row_is_eligible(&empty),
                "self-heal accepted {reason}"
            );
        }

        let contended = HashSet::from([base.contended_capture_key()]);
        assert!(!base.self_heal_row_is_eligible(&contended));
    }

    // Tests for agent_session_id field
    use tempfile::tempdir;

    // Tests for agent_session_id field
    #[test]
    fn test_agent_session_id_none_by_default() {
        let inst = Instance::new("test", "/tmp/test");
        assert!(inst.agent_session_id.is_none());
    }

    #[test]
    fn test_agent_session_id_serialization() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.agent_session_id = Some("session-123".to_string());

        let json = serde_json::to_string(&inst).unwrap();
        let deserialized: Instance = serde_json::from_str(&json).unwrap();

        assert_eq!(
            deserialized.agent_session_id,
            Some("session-123".to_string())
        );
    }

    #[test]
    fn test_agent_session_id_skips_none() {
        let inst = Instance::new("test", "/tmp/test");
        let json = serde_json::to_string(&inst).unwrap();

        // agent_session_id should not appear in JSON when None
        assert!(!json.contains("agent_session_id"));
    }

    #[test]
    fn test_agent_session_id_defaults_to_none() {
        let json = r#"{"id":"test123","title":"Test","project_path":"/tmp/test","group_path":"","command":"","tool":"claude","yolo_mode":false,"status":"idle","created_at":"2024-01-01T00:00:00Z"}"#;
        let inst: Instance = serde_json::from_str(json).unwrap();

        assert!(inst.agent_session_id.is_none());
    }

    #[test]
    fn test_persisted_opencode_session_id_reused() {
        let mut inst = Instance::new("Test", "/tmp/test");
        inst.tool = "opencode".to_string();
        inst.agent_session_id = Some("oc-session-42".to_string());

        let (session_id, is_existing) = inst.acquire_session_id();

        assert_eq!(session_id, Some("oc-session-42".to_string()));
        assert!(is_existing);
    }
    #[test]
    #[serial_test::serial]
    fn persisted_native_id_roundtrip_resumes_the_same_conversation() {
        let temp = tempfile::tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(temp.path());
        let claude_home = temp.path().join(".claude");
        let _claude = crate::session::test_support::EnvGuard::set(&[(
            "CLAUDE_CONFIG_DIR",
            claude_home.clone(),
        )]);
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let project = std::fs::canonicalize(project).unwrap();
        let native_id = "11111111-2222-4333-8444-555555555555";
        let transcript = claude_home
            .join("projects")
            .join(crate::session::capture::encode_claude_project_path(
                &project.to_string_lossy(),
            ))
            .join(format!("{native_id}.jsonl"));
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::write(transcript, "conversation").unwrap();

        let mut inst = Instance::new("Test", &project.to_string_lossy());
        inst.tool = "claude".to_string();
        inst.agent_session_id = Some(native_id.to_string());
        let json = serde_json::to_string(&inst).unwrap();
        let mut reloaded: Instance = serde_json::from_str(&json).unwrap();

        let (session_id, is_existing) = reloaded.acquire_session_id();
        assert_eq!(session_id.as_deref(), Some(native_id));
        assert!(is_existing);
        assert_eq!(
            crate::session::instance::launch_command::build_resume_flags(
                "claude",
                session_id.as_deref().unwrap(),
                is_existing,
            ),
            format!("--resume {native_id}")
        );
    }

    #[test]
    fn test_persisted_session_id_reused_when_already_set() {
        let mut inst = Instance::new("Test", "/tmp/test");
        inst.tool = "claude".to_string();
        inst.agent_session_id = Some("session-42".to_string());

        // A persisted sid is returned as the session this instance owns. The
        // `--resume` vs `--session-id` decision (is_existing) is
        // transcript-dependent for Claude and is covered hermetically in
        // `verify_on_resume`; asserting it here would read the developer's real
        // `~/.claude`.
        let (session_id, _is_existing) = inst.acquire_session_id();
        assert_eq!(session_id, Some("session-42".to_string()));
    }

    #[test]
    fn test_persisted_session_id_reused_for_unsupported_agent() {
        // The cache-hit path is generic across agents; a persisted ID is
        // returned regardless of whether the agent supports resume yet.
        let mut inst = Instance::new("Test", "/tmp/test");
        inst.tool = "codex".to_string();
        inst.agent_session_id = Some("sess-99".to_string());

        let (session_id, is_existing) = inst.acquire_session_id();

        assert_eq!(session_id, Some("sess-99".to_string()));
        assert!(is_existing);
    }

    #[test]
    fn test_resume_with_arbitrary_session_id() {
        let mut inst = Instance::new("Test", "/home/user/project");
        inst.tool = "claude".to_string();
        inst.agent_session_id = Some("invalid-session-id".to_string());

        // With an existing (persisted) session, should use --resume
        let flags = build_resume_flags(&inst.tool, inst.agent_session_id.as_ref().unwrap(), true);
        assert_eq!(flags, "--resume invalid-session-id");

        // A fresh (no prior transcript) launch pins the id instead.
        let flags = build_resume_flags(&inst.tool, inst.agent_session_id.as_ref().unwrap(), false);
        assert_eq!(flags, "--session-id invalid-session-id");

        // The method returns the persisted id as the owned session. The
        // is_existing flag is transcript-dependent for Claude (see
        // `verify_on_resume`) and would read the real `~/.claude` here.
        let (session_id, _is_existing) = inst.acquire_session_id();
        assert_eq!(session_id, Some("invalid-session-id".to_string()));
    }

    #[test]
    fn fork_intent_emits_resume_fork_session_and_pins_child() {
        let flags = build_fork_flags(
            "claude",
            "parent-1111-2222-3333-444444444444",
            "child-5555-6666-7777-888888888888",
        );
        assert_eq!(
            flags,
            "--resume parent-1111-2222-3333-444444444444 --fork-session --session-id child-5555-6666-7777-888888888888"
        );
    }

    #[test]
    fn acquire_session_id_fork_pins_child_and_reports_fresh() {
        let mut inst = Instance::new("Forked", "/tmp/x");
        inst.tool = "claude".to_string();
        // The child id was pre-generated and stored in agent_session_id at
        // creation; the Fork intent carries the parent to resume from.
        inst.agent_session_id = Some("child-5555-6666-7777-888888888888".to_string());
        inst.resume_intent = ResumeIntent::Fork {
            from: "parent-1111-2222-3333-444444444444".to_string(),
        };
        let mut cmd = "claude".to_string();
        let is_existing = inst.apply_session_flags(&mut cmd, "test").unwrap();
        assert_eq!(
            cmd,
            "claude --resume parent-1111-2222-3333-444444444444 --fork-session --session-id child-5555-6666-7777-888888888888"
        );
        // A fork is a NEW session (not a resume-in-place), so report not-existing.
        assert!(!is_existing);
        // The child id we will resume from here on stays pinned in agent_session_id.
        assert_eq!(
            inst.agent_session_id.as_deref(),
            Some("child-5555-6666-7777-888888888888")
        );
    }

    #[test]
    fn sandbox_resume_flags_follow_capture_context_support() {
        let sid = "11111111-2222-3333-4444-555555555555";
        for (tool, expected, resumed) in [
            ("copilot", format!("copilot --session-id {sid}"), true),
            ("kimi", format!("kimi --session {sid}"), true),
            ("prime-agent", format!("prime-agent --resume {sid}"), true),
        ] {
            let mut inst = Instance::new("test", "/tmp/test");
            inst.tool = tool.to_string();
            inst.agent_session_id = Some(sid.to_string());
            inst.resume_intent = ResumeIntent::Use(sid.to_string());
            inst.sandbox_info = Some(SandboxInfo {
                enabled: true,
                container_id: None,
                image: "test-image".to_string(),
                container_name: "test".to_string(),
                extra_env: None,
                custom_instruction: None,
                before_start_env: Vec::new(),
                container_workdir: None,
            });
            let mut cmd = tool.to_string();
            assert_eq!(
                inst.apply_session_flags(&mut cmd, "test").unwrap(),
                resumed,
                "{tool}"
            );
            assert_eq!(cmd, expected, "{tool}");
            assert_eq!(inst.agent_session_id.as_deref(), Some(sid));
        }

        let mut automatic_copilot = Instance::new("test", "/tmp/test");
        automatic_copilot.tool = "copilot".to_string();
        automatic_copilot.agent_session_id = Some(sid.to_string());
        automatic_copilot.sandbox_info = Some(SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test-image".to_string(),
            container_name: "test".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
        });
        let mut automatic_cmd = "copilot".to_string();
        assert!(!automatic_copilot
            .apply_session_flags(&mut automatic_cmd, "test")
            .unwrap());
        assert_eq!(automatic_cmd, "copilot");

        let mut host_prime = Instance::new("test", "/tmp/test");
        host_prime.tool = "prime-agent".to_string();
        host_prime.agent_session_id = Some(sid.to_string());
        host_prime.resume_intent = ResumeIntent::Use(sid.to_string());
        let mut cmd = "prime-agent".to_string();
        assert!(host_prime.apply_session_flags(&mut cmd, "test").unwrap());
        assert_eq!(cmd, format!("prime-agent --resume {sid}"));
    }
    #[test]
    #[serial_test::serial]
    fn fresh_launch_clears_every_host_identity_sidecar() {
        let tmp = tempfile::TempDir::new().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(&tmp.path().join("app"));
        for tool in ["cursor", "pi"] {
            let mut inst = Instance::new(tool, "/tmp/test");
            inst.tool = tool.to_string();
            inst.detect_as = tool.to_string();
            crate::hooks::write_session_id_via_guard(&inst.id, "stale-sid").unwrap();
            assert!(crate::hooks::session_id_sidecar_exists(&inst.id));

            inst.clear_pane_identity_sidecar();
            assert!(
                !crate::hooks::session_id_sidecar_exists(&inst.id),
                "{tool} retained stale pane identity"
            );
        }
    }
    #[test]
    #[serial_test::serial]
    fn sandboxed_prime_agent_uses_only_its_managed_store_poller() {
        let tmp = tempfile::TempDir::new().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(&tmp.path().join("app"));
        let mut inst = Instance::new("test", "/tmp/test");
        inst.tool = "prime-agent".to_string();
        inst.sandbox_info = Some(SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test-image".to_string(),
            container_name: "test".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: Some("/workspace/test".to_string()),
        });

        assert_eq!(inst.try_retroactive_capture(), None);
        std::fs::create_dir_all(inst.sandbox_capture_store_dir().unwrap()).unwrap();
        inst.capture_started_at = Some(std::time::SystemTime::now());
        inst.maybe_start_poller_since(None);
        assert!(inst.session_id_poller.is_some());
    }

    #[test]
    #[serial_test::serial]
    fn prime_capture_plan_follows_upstream_session_directory_precedence() {
        let tmp = tempfile::tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(&tmp.path().join("app"));
        let project = tmp.path().join("project");
        std::fs::create_dir_all(project.join(".prime/agent")).unwrap();
        let mut inst = Instance::new("prime-plan", project.to_str().unwrap());
        inst.tool = "prime-agent".to_string();
        inst.sandbox_info = Some(SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test-image".to_string(),
            container_name: "prime-plan".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: Some("/workspace/project".to_string()),
        });
        let store = inst.sandbox_capture_store_dir().unwrap();
        std::fs::create_dir_all(&store).unwrap();
        let mut config = inst.build_container_config().unwrap();

        let plan = inst
            .prime_agent_capture_plan_with(&config, store.clone())
            .unwrap();
        assert_eq!(plan.session_dir, Path::new("sessions"));

        inst.extra_args = "--mode daemon --mode invalid".to_string();
        assert!(inst
            .prime_agent_capture_plan_with(&config, store.clone())
            .is_none());
        inst.extra_args.clear();

        std::fs::write(
            store.join("settings.json"),
            serde_json::json!({"sessionDir": "~/.prime/agent/global"}).to_string(),
        )
        .unwrap();
        let plan = inst
            .prime_agent_capture_plan_with(&config, store.clone())
            .unwrap();
        assert_eq!(plan.session_dir, Path::new("global"));

        std::fs::write(
            project.join(".prime/agent/settings.json"),
            serde_json::json!({"sessionDir": "~/.prime/agent/project"}).to_string(),
        )
        .unwrap();
        let plan = inst
            .prime_agent_capture_plan_with(&config, store.clone())
            .unwrap();
        assert_eq!(plan.session_dir, Path::new("project"));

        config
            .anonymous_volumes
            .push("/workspace/project/.prime".to_string());
        assert!(inst
            .prime_agent_capture_plan_with(&config, store.clone())
            .is_none());
        config.anonymous_volumes.clear();
        std::fs::write(store.join("settings.json"), "{").unwrap();
        let plan = inst
            .prime_agent_capture_plan_with(&config, store.clone())
            .unwrap();
        assert_eq!(plan.session_dir, Path::new("project"));
        std::fs::write(
            store.join("settings.json"),
            serde_json::json!({"sessionDir": "~/.prime/agent/global"}).to_string(),
        )
        .unwrap();
        std::fs::write(project.join(".prime/agent/settings.json"), "[]").unwrap();
        let plan = inst
            .prime_agent_capture_plan_with(&config, store.clone())
            .unwrap();
        assert_eq!(plan.session_dir, Path::new("global"));
        std::fs::write(
            project.join(".prime/agent/settings.json"),
            serde_json::json!({"sessionDir": "~/.prime/agent/project"}).to_string(),
        )
        .unwrap();
        config
            .environment
            .push(crate::containers::EnvEntry::Literal {
                key: "PRIME_AGENT_CODING_AGENT_SESSION_DIR".to_string(),
                value: "~/.prime/agent/legacy".to_string(),
            });
        let plan = inst
            .prime_agent_capture_plan_with(&config, store.clone())
            .unwrap();
        assert_eq!(plan.session_dir, Path::new("legacy"));

        config
            .environment
            .push(crate::containers::EnvEntry::Literal {
                key: "PRIME_AGENT_SESSION_DIR".to_string(),
                value: "~/.prime/agent/current".to_string(),
            });
        let plan = inst
            .prime_agent_capture_plan_with(&config, store.clone())
            .unwrap();
        assert_eq!(plan.session_dir, Path::new("current"));

        config
            .environment
            .retain(|entry| entry.key() != "PRIME_AGENT_SESSION_DIR");
        config
            .environment
            .push(crate::containers::EnvEntry::Literal {
                key: "PRIME_AGENT_SESSION_DIR".to_string(),
                value: String::new(),
            });
        let plan = inst
            .prime_agent_capture_plan_with(&config, store.clone())
            .unwrap();
        assert_eq!(plan.session_dir, Path::new("project"));

        config.environment.retain(|entry| {
            !matches!(
                entry.key(),
                "PRIME_AGENT_SESSION_DIR" | "PRIME_AGENT_CODING_AGENT_SESSION_DIR"
            )
        });
        inst.sandbox_info.as_mut().unwrap().container_workdir =
            Some("/root/.prime/agent/work".to_string());
        let work_settings = store.join("work/.prime/agent/settings.json");
        std::fs::create_dir_all(work_settings.parent().unwrap()).unwrap();
        std::fs::write(
            &work_settings,
            serde_json::json!({"sessionDir": ""}).to_string(),
        )
        .unwrap();
        let plan = inst
            .prime_agent_capture_plan_with(&config, store.clone())
            .unwrap();
        assert_eq!(plan.session_dir, Path::new("work"));
        std::fs::write(
            &work_settings,
            serde_json::json!({"sessionDir": null}).to_string(),
        )
        .unwrap();
        let plan = inst
            .prime_agent_capture_plan_with(&config, store.clone())
            .unwrap();
        assert_eq!(plan.session_dir, Path::new("sessions"));

        inst.sandbox_info.as_mut().unwrap().container_workdir =
            Some("/workspace/project".to_string());
        for option in [
            "--autonomous-gate",
            "--autonomous-gate-retries",
            "--autonomous-gate-timeout-ms",
            "--autonomous-max-continuations",
            "--autonomous-max-turns",
            "--autonomous-max-tokens",
            "--autonomous-timeout-ms",
        ] {
            inst.extra_args = format!("{option} --session-dir");
            let plan = inst
                .prime_agent_capture_plan_with(&config, store.clone())
                .unwrap_or_else(|| panic!("{option} value was parsed as a session option"));
            assert_eq!(plan.session_dir, Path::new("project"));
        }

        inst.extra_args = "--mode daemon".to_string();
        assert!(inst
            .prime_agent_capture_plan_with(&config, store.clone())
            .is_none());

        inst.extra_args =
            "--autonomous --cwd /root/.prime/agent/work/../work --session-dir ../cli".to_string();
        let plan = inst
            .prime_agent_capture_plan_with(&config, store.clone())
            .unwrap();
        assert_eq!(plan.container_cwd, "/root/.prime/agent/work");
        assert_eq!(plan.session_dir, Path::new("cli"));

        inst.extra_args = "--no-session".to_string();
        assert!(inst
            .prime_agent_capture_plan_with(&config, store.clone())
            .is_none());
        inst.extra_args = "--session-dir /tmp/outside".to_string();
        assert!(inst
            .prime_agent_capture_plan_with(&config, store.clone())
            .is_none());

        inst.extra_args = "--session-dir ~/masked".to_string();
        config.volumes.push(crate::containers::VolumeMount {
            host_path: tmp.path().join("masked").to_string_lossy().into_owned(),
            container_path: "/root/.prime/agent/masked".to_string(),
            read_only: true,
        });
        assert!(inst.prime_agent_capture_plan_with(&config, store).is_none());
    }
    #[test]
    #[serial_test::serial]
    fn prime_extension_root_only_keeps_parent_publication() {
        if which::which("node").is_err() {
            eprintln!("skipping: node not found on PATH");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(&tmp.path().join("app"));
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let mut inst = Instance::new("prime-root-only", project.to_str().unwrap());
        inst.tool = "prime-agent".to_string();
        inst.extra_args = "--session-dir /root/.prime/agent/custom-sessions".to_string();
        inst.sandbox_info = Some(SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test-image".to_string(),
            container_name: "prime-root-only".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: Some("/workspace/project".to_string()),
        });

        let (launch, _, _, _) = inst.build_launch_command().unwrap();
        let launch = launch.unwrap();
        assert!(
            launch.contains("/root/.prime/agent/extensions/aoe-session-id.js"),
            "{launch}"
        );
        assert!(launch.contains("AOE_SESSION_ROOT_ONLY=1"));
        assert!(launch.contains(" -e PRIME_AGENT_CODING_AGENT_DIR='/root/.prime/agent'"));
        assert!(!launch.contains("--session-id"));

        let store = inst.sandbox_capture_store_dir().unwrap();
        assert_eq!(
            std::fs::read(store.join("extensions/aoe-session-id.js")).unwrap(),
            SESSION_IDENTITY_EXTENSION.as_bytes()
        );
        let sessions = store.join("custom-sessions");
        std::fs::create_dir_all(sessions.join("children")).unwrap();
        let parent_id = "018f47a6-7b80-7cc3-98a2-37b5f486b2a1";
        let child_id = "018f47a6-7b80-7cc3-98a2-37b5f486b2a2";
        std::fs::write(
            sessions.join("parent.jsonl"),
            format!(
                "{}\n",
                serde_json::json!({
                    "type": "session",
                    "version": 3,
                    "id": parent_id,
                    "timestamp": "2026-09-05T00:00:00.000Z",
                    "cwd": "/workspace/project",
                    "rlmDepth": 0,
                })
            ),
        )
        .unwrap();
        let child_header = format!(
            "{}\n",
            serde_json::json!({
                "type": "session",
                "version": 3,
                "id": child_id,
                "timestamp": "2026-09-05T00:00:01.000Z",
                "cwd": "/workspace/project",
                "rlmDepth": 1,
            })
        );
        std::fs::write(sessions.join("children/child.jsonl"), &child_header).unwrap();
        std::fs::write(sessions.join("child.jsonl"), child_header).unwrap();
        let sidecar = store.join("aoe-session").join(&inst.id).join("session_id");
        let path_sidecar = sidecar.parent().unwrap().join("session_path");
        let default_sidecar = tmp.path().join("pi-default/session_id");
        let default_path_sidecar = default_sidecar.parent().unwrap().join("session_path");
        let script = r#"
import { readFileSync } from "node:fs";
import { pathToFileURL } from "node:url";

const extension = (await import(pathToFileURL(process.argv[1]).href)).default;
process.chdir(process.argv[4]);
const context = (id, path, rlmDepth) => ({
  sessionManager: {
    getSessionId: () => id,
    getSessionFile: () => path,
    getHeader: () => ({ rlmDepth }),
  },
});
async function publish(target, rootOnly) {
  process.env.AOE_SESSION_ID_FILE = target;
  if (rootOnly) process.env.AOE_SESSION_ROOT_ONLY = "1";
  else process.env.AOE_SESSION_ROOT_ONLY = "0";
  let sessionStart;
  extension({ on(name, handler) { if (name === "session_start") sessionStart = handler; } });
  await sessionStart({}, context(
    "018f47a6-7b80-7cc3-98a2-37b5f486b2a1",
    "custom-sessions/parent.jsonl",
    0,
  ));
  await sessionStart({}, context(
    "018f47a6-7b80-7cc3-98a2-37b5f486b2a2",
    "custom-sessions/children/child.jsonl",
    undefined,
  ));
  await sessionStart({}, context(
    "018f47a6-7b80-7cc3-98a2-37b5f486b2a2",
    "custom-sessions/children/child.jsonl",
    1,
  ));
  return readFileSync(target, "utf8").trim();
}
const rootOnly = await publish(process.argv[2], true);
const defaultMode = await publish(process.argv[3], false);
process.stdout.write(JSON.stringify({ rootOnly, defaultMode }));
"#;
        let output = std::process::Command::new("node")
            .args(["--input-type=module", "--eval", script])
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/session/aoe-session-id.js"))
            .arg(&sidecar)
            .arg(&default_sidecar)
            .arg(&store)
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let published: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(published["rootOnly"], parent_id);
        assert_eq!(
            published["defaultMode"], child_id,
            "Pi default behavior changed"
        );
        assert_eq!(
            std::fs::read_to_string(&default_path_sidecar)
                .unwrap()
                .trim(),
            "custom-sessions/children/child.jsonl",
            "Pi default path publication changed"
        );
        assert_eq!(
            std::fs::read_to_string(&path_sidecar).unwrap().trim(),
            store.join("custom-sessions/parent.jsonl").to_str().unwrap()
        );

        std::fs::write(&sidecar, child_id).unwrap();
        std::fs::write(
            &path_sidecar,
            "/root/.prime/agent/custom-sessions/child.jsonl",
        )
        .unwrap();
        assert_eq!(
            inst.prime_published_root_session_id(),
            None,
            "a direct child transcript must fail root validation"
        );
        std::fs::write(&sidecar, parent_id).unwrap();
        std::fs::write(
            &path_sidecar,
            "/root/.prime/agent/custom-sessions/parent.jsonl",
        )
        .unwrap();

        let mut restarted: Instance =
            serde_json::from_str(&serde_json::to_string(&inst).unwrap()).unwrap();
        let mut command = "prime-agent".to_string();
        assert!(restarted.apply_session_flags(&mut command, "test").unwrap());
        assert_eq!(command, format!("prime-agent --resume {parent_id}"));

        let prepared = restarted.prepare_launch_command().unwrap();
        assert!(prepared.command.as_deref().unwrap().contains(parent_id));
        let newer_id = "018f47a6-7b80-7cc3-98a2-37b5f486b2a3";
        std::fs::write(
            sessions.join("newer.jsonl"),
            format!(
                "{}\n",
                serde_json::json!({
                    "type": "session",
                    "version": 3,
                    "id": newer_id,
                    "timestamp": "2026-09-05T00:00:02.000Z",
                    "cwd": "/workspace/project",
                    "rlmDepth": 0,
                })
            ),
        )
        .unwrap();
        std::fs::write(&sidecar, newer_id).unwrap();
        std::fs::write(
            &path_sidecar,
            "/root/.prime/agent/custom-sessions/newer.jsonl",
        )
        .unwrap();
        let prepared = restarted
            .refresh_prepared_prime_launch_after_quiescence(prepared)
            .unwrap();
        assert_eq!(restarted.agent_session_id.as_deref(), Some(newer_id));
        assert!(prepared.command.as_deref().unwrap().contains(newer_id));
    }
    #[test]
    fn clearing_the_conversation_drops_its_transcript_path() {
        let mut inst = Instance::new("pi-clear", "/tmp/pi-clear");
        inst.tool = "pi".to_string();
        inst.agent_session_id = Some("aaaaaaaa-1111-4111-8111-aaaaaaaaaaaa".to_string());
        inst.pi_session_path = Some(
            "/store/2026-01-01T00-00-00-000Z_aaaaaaaa-1111-4111-8111-aaaaaaaaaaaa.jsonl"
                .to_string(),
        );
        inst.resume_intent = ResumeIntent::Cleared;

        let (sid, is_existing) = inst.acquire_session_id_with(&|_| None);

        assert_eq!(sid, None, "no pin without a mint seam");
        assert!(!is_existing);
        assert_eq!(
            inst.pi_session_path, None,
            "the dropped conversation's transcript must not linger"
        );
    }

    // A reload keeps the row and drops the runtime flag, so the file is all
    // that says this pane publishes. Looking in the host hook dir for a
    // container's sidecar reads a live publisher as a silent one: no poller
    // repair, and a flush that returns before reading anything.
    // An unresolvable sandbox path must not read as "use the host one": that
    // is a conversation from another namespace, which is the attribution bug
    // this change removes.
    #[test]
    #[serial_test::serial]
    fn an_unresolvable_sandbox_source_fails_closed() {
        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::EnvGuard::set(&[("HOME", temp.path())]);

        // An id the dir guard refuses is one way the path cannot resolve.
        let mut inst = Instance::new("pi-unresolvable", "/tmp/pi-unresolvable");
        inst.id = "../escape".to_string();
        inst.tool = "pi".to_string();
        inst.sandbox_info = Some(crate::session::SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test:latest".to_string(),
            container_name: "aoe-pi-unresolvable".to_string(),
            extra_env: None,
            custom_instruction: None,
            container_workdir: None,
            before_start_env: Vec::new(),
        });

        assert_eq!(
            inst.pi_sidecar_source(),
            None,
            "no source is the safe answer"
        );
        assert!(
            !inst.uses_pi_session_sidecar(),
            "a pane with no resolvable source does not publish"
        );
        assert!(
            !inst.supports_session_poller(),
            "and must not poll, which would read the host sidecar"
        );
        assert_eq!(inst.pi_published_session_id(true), None);
        assert_eq!(inst.pi_published_session_path(), None);

        // The host pane it must not be confused with does have a source.
        let mut host = Instance::new("pi-host-src", "/tmp/pi-unresolvable");
        host.tool = "pi".to_string();
        assert_eq!(
            host.pi_sidecar_source(),
            Some(SessionSidecarSource::HostHooks)
        );
    }

    #[test]
    #[serial_test::serial]
    fn reloaded_sandbox_session_still_finds_its_sidecar() {
        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::EnvGuard::set(&[("HOME", temp.path())]);

        let mut inst = Instance::new("pireloadsandbox01", "/tmp/pi-reload");
        inst.tool = "pi".to_string();
        inst.sandbox_info = Some(crate::session::SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test:latest".to_string(),
            container_name: "aoe-pi-reload".to_string(),
            extra_env: None,
            custom_instruction: None,
            container_workdir: None,
            before_start_env: Vec::new(),
        });
        inst.mark_pi_extension_launched_for_test();

        // Round-trip the way a daemon or TUI reload does.
        let reloaded: Instance =
            serde_json::from_str(&serde_json::to_string(&inst).unwrap()).unwrap();
        assert!(
            !reloaded.uses_pi_session_sidecar(),
            "nothing published yet, so nothing to find"
        );

        let dir = reloaded
            .pi_sidecar_source()
            .and_then(|s| match s {
                crate::session::instance::SessionSidecarSource::SandboxDir(d) => Some(d),
                _ => None,
            })
            .expect("a sandboxed pane has a bind-backed sidecar");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("session_id"),
            "01a053b6-c470-78de-9d8f-bc00ef05332a\n",
        )
        .unwrap();

        assert!(
            reloaded.uses_pi_session_sidecar(),
            "the published file is what a reloaded session has to go on"
        );
        assert!(
            reloaded.supports_session_poller(),
            "poller repair must stay available after a reload"
        );
        assert_eq!(
            reloaded.pi_published_session_id(true).as_deref(),
            Some("01a053b6-c470-78de-9d8f-bc00ef05332a"),
            "and the final flush must read it"
        );
    }

    #[test]
    #[serial_test::serial]
    #[cfg(unix)]
    fn sandbox_pi_sidecar_reads_are_bounded_and_nonblocking() {
        use nix::sys::stat::Mode;
        use nix::unistd::mkfifo;

        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::EnvGuard::set(&[("HOME", temp.path())]);
        let mut inst = Instance::new("piboundedsidecar", "/tmp/pi-bounded");
        inst.tool = "pi".to_string();
        inst.sandbox_info = Some(crate::session::SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test:latest".to_string(),
            container_name: "aoe-pi-bounded".to_string(),
            extra_env: None,
            custom_instruction: None,
            container_workdir: None,
            before_start_env: Vec::new(),
        });
        let SessionSidecarSource::SandboxDir(dir) = inst.pi_sidecar_source().unwrap() else {
            panic!("sandboxed Pi must publish into its config bind");
        };
        std::fs::create_dir_all(&dir).unwrap();
        let sidecar = dir.join("session_id");
        std::fs::write(&sidecar, vec![b'x'; SESSION_SIDECAR_MAX_BYTES + 1]).unwrap();
        assert_eq!(inst.pi_published_session_id(true), None);

        std::fs::remove_file(&sidecar).unwrap();
        mkfifo(&sidecar, Mode::S_IRUSR | Mode::S_IWUSR).unwrap();
        assert_eq!(inst.pi_published_session_id(true), None);
        let poll = crate::session::capture::pi_sidecar_poll_fn(
            inst.id.clone(),
            SessionSidecarSource::SandboxDir(dir.clone()),
        );
        assert!(poll().is_none());

        std::fs::remove_file(&sidecar).unwrap();
        let root = dir.parent().and_then(std::path::Path::parent).unwrap();
        std::fs::remove_dir_all(root.join("aoe-session")).unwrap();
        let foreign = temp.path().join("foreign-aoe-session");
        std::fs::create_dir_all(foreign.join(&inst.id)).unwrap();
        std::fs::write(
            foreign.join(&inst.id).join("session_id"),
            "99999999-9999-4999-8999-999999999999",
        )
        .unwrap();
        std::os::unix::fs::symlink(&foreign, root.join("aoe-session")).unwrap();
        assert!(
            poll().is_none(),
            "the poller must anchor above the replaceable aoe-session ancestor"
        );
    }

    /// A Pi session pointed at a config dir the user named gets no sidecar,
    /// neither writing one nor reading one.
    ///
    /// Both halves of the path (the discovered extension, the published file)
    /// live in the bind AoE mounts itself; the user's own dir reaches the
    /// container through their `extra_volumes` entry, at a path AoE cannot
    /// see. The read gate is the half that matters after the fact: the bind
    /// stays mounted and writable from the container, so a sidecar left there
    /// by an earlier launch (or written by the pane itself) would otherwise
    /// resume this session onto a conversation it never published.
    #[test]
    #[serial_test::serial]
    fn sandboxed_pi_with_its_own_config_dir_uses_its_mounted_sidecar() {
        const STALE_ID: &str = "01a053b6-c470-78de-9d8f-bc00ef05332a";
        let _guard = crate::session::test_support::isolate_app_dir();
        let app_dir = crate::session::get_app_dir().unwrap();

        let sandboxed_pi = |id: &str| {
            let mut inst = Instance::new(id, "/tmp/pi-own-config");
            inst.tool = "pi".to_string();
            inst.sandbox_info = Some(crate::session::SandboxInfo {
                enabled: true,
                container_id: None,
                image: "test:latest".to_string(),
                container_name: "aoe-pi-own-config".to_string(),
                extra_env: None,
                custom_instruction: None,
                container_workdir: None,
                before_start_env: Vec::new(),
            });
            inst
        };

        let inst = sandboxed_pi("piownconfig01");
        let SessionSidecarSource::SandboxDir(stale_sidecar) = inst.pi_sidecar_source().unwrap()
        else {
            panic!("sandboxed Pi must publish into its config bind");
        };
        std::fs::create_dir_all(&stale_sidecar).unwrap();
        std::fs::write(stale_sidecar.join("session_id"), format!("{STALE_ID}\n")).unwrap();

        std::fs::write(
            app_dir.join("config.toml"),
            r#"[session.agent_config_dir]
pi = "~/.pi-personal"
"#,
        )
        .unwrap();

        let mut declared = sandboxed_pi("piownconfig01");
        let (_, env_prefix) = declared
            .identity_extension_launch()
            .expect("declared sandbox config supports the pane extension");
        assert!(env_prefix.contains("AOE_SESSION_ID_FILE=/root/.pi/aoe-session/"));
        declared.mark_pi_extension_launched_for_test();
        assert!(declared.pi_sidecar_source().is_some());
        assert!(declared.uses_pi_session_sidecar());
        assert_eq!(declared.pi_published_session_id(true), None);

        let mut cmd = String::from("pi");
        declared.apply_session_flags(&mut cmd, "test").unwrap();
        assert!(
            !cmd.contains(STALE_ID) && !cmd.contains("--session"),
            "a sidecar from the unmounted default store must not reach the launch line: {cmd:?}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn sandbox_transcript_paths_validate_in_the_host_namespace() {
        // The container publishes `/root/.pi/...`; the file lives under the
        // sandbox dir on this side. Checking the container path verbatim would
        // reject every sandbox transcript.
        let mut inst = Instance::new("pi-ns", "/tmp/pi-ns");
        inst.tool = "pi".to_string();
        inst.sandbox_info = Some(crate::session::SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test-image".to_string(),
            container_name: "aoe-pi-ns".to_string(),
            extra_env: None,
            custom_instruction: None,
            container_workdir: None,
            before_start_env: Vec::new(),
        });

        let published = "/root/.pi/sessions/--proj--/2026-01-01T00-00-00-000Z_x.jsonl";
        let host = inst
            .pi_host_view_of(published)
            .expect("a container path maps to the sandbox dir");
        let sandbox_root = inst.sandbox_capture_store_dir().unwrap();
        assert!(host.starts_with(&sandbox_root),);
        assert!(host.ends_with("sessions/--proj--/2026-01-01T00-00-00-000Z_x.jsonl"));
        assert_eq!(
            inst.pi_host_view_of("/elsewhere/x.jsonl"),
            None,
            "a path outside the bind cannot be mapped"
        );

        // A host pane's path is already a host path.
        let mut host_inst = Instance::new("pi-host-ns", "/tmp/pi-ns");
        host_inst.tool = "pi".to_string();
        assert_eq!(
            host_inst.pi_host_view_of("/home/u/.pi/x.jsonl"),
            Some(std::path::PathBuf::from("/home/u/.pi/x.jsonl"))
        );
    }

    #[test]
    fn pi_resumes_by_published_path_only_for_its_own_transcript() {
        // The path is what survives a worktree move, but a path left over from
        // a previous conversation must not resume it, so the file name has to
        // carry the id the row holds.
        let temp = tempfile::tempdir().unwrap();
        let id = "aaaaaaaa-1111-4111-8111-aaaaaaaaaaaa";
        let other = "bbbbbbbb-2222-4222-8222-bbbbbbbbbbbb";
        let mine = temp
            .path()
            .join(format!("2026-01-01T00-00-00-000Z_{id}.jsonl"));
        std::fs::write(&mine, "{}\n").unwrap();
        let theirs = temp
            .path()
            .join(format!("2026-01-01T00-00-00-000Z_{other}.jsonl"));
        std::fs::write(&theirs, "{}\n").unwrap();

        let mut inst = Instance::new("pi-path", "/tmp/pi-path");
        inst.tool = "pi".to_string();
        inst.agent_session_id = Some(id.to_string());

        assert_eq!(
            inst.pi_resumable_transcript(),
            None,
            "no path published yet"
        );

        inst.pi_session_path = Some(mine.to_string_lossy().to_string());
        assert_eq!(
            inst.pi_resumable_transcript().as_deref(),
            Some(mine.to_string_lossy().as_ref()),
            "the pane's own transcript resumes by path"
        );

        inst.pi_session_path = Some(theirs.to_string_lossy().to_string());
        assert_eq!(
            inst.pi_resumable_transcript(),
            None,
            "a path for another conversation must not be resumed"
        );

        // A partial pin, which `set-session-id` accepts, must not match by
        // substring: the id segment has to be the whole uuid.
        inst.agent_session_id = Some("aaaaaaaa".to_string());
        inst.pi_session_path = Some(mine.to_string_lossy().to_string());
        assert_eq!(inst.pi_resumable_transcript(), None, "partial pin");
        inst.agent_session_id = Some(id.to_string());

        inst.pi_session_path = Some(
            temp.path()
                .join(format!("2026-01-01T00-00-00-000Z_{id}.jsonl.gone"))
                .to_string_lossy()
                .to_string(),
        );
        assert_eq!(inst.pi_resumable_transcript(), None, "the file must exist");
    }

    #[test]
    fn pi_relaunch_of_an_unwritten_pin_uses_the_creating_flag() {
        // pi writes its session file on the first message, so a pane that was
        // pinned and never prompted has an id the store has never recorded.
        // `--session` exits 1 on such an id; the pinning arm recreates it.
        let mut inst = Instance::new("pi-pinned", "/tmp/pi-pinned");
        inst.tool = "pi".to_string();

        let minted = Some("aaaaaaaa-1111-4111-8111-aaaaaaaaaaaa");
        // (label, pinnable, id, user-pinned) -> takes the `existing` arm.
        // `--session-id` creates when the id is absent and searches this
        // project only, so it is for ids AoE minted or captured; anything the
        // user handed us keeps `--session`, which resolves partials and
        // searches wider.
        for (label, pinnable, sid, explicit, expected) in [
            ("minted, pinnable", true, minted, false, false),
            ("minted, old binary", false, minted, true, true),
            ("user-pinned partial", true, Some("aaaaaaaa"), true, true),
            ("user-pinned full uuid", true, minted, true, true),
            ("no id", false, None, false, false),
        ] {
            assert_eq!(
                inst.resume_flag_arm_is_existing(sid.is_some(), pinnable, sid, explicit),
                expected,
                "{label}"
            );
        }

        // Every other agent tracks is_existing whatever the pi probe says,
        // and never reaches the probe: `pi_session_id_pinnable` is gated on
        // the tool so no other launch spawns `pi --help`.
        let mut claude = Instance::new("claude-pinned", "/tmp/pi-pinned");
        claude.tool = "claude".to_string();
        assert!(claude.resume_flag_arm_is_existing(true, true, minted, false));
        assert!(!claude.pi_session_id_pinnable());
    }

    #[test]
    fn test_acquire_session_id_idempotence() {
        let mut inst = Instance::new("Test", "/tmp/test");
        inst.tool = "claude".to_string();

        let (first, first_existing) = inst.acquire_session_id();
        let (second, second_existing) = inst.acquire_session_id();

        // Repeated acquire yields a STABLE id. The first mint reports fresh; a
        // second acquire with no transcript on disk stays fresh-pinned (an empty
        // thread's sid is not resumable) but returns the same id, so a later
        // relaunch keeps `--session-id <same>` rather than a doomed `--resume`.
        assert!(first.is_some());
        assert!(!first_existing);
        assert!(!second_existing);
        assert_eq!(first, second);
    }

    #[test]
    fn opencode_fresh_arm_uses_preassign_seam() {
        // opencode's fresh launch adopts the id the preassign seam returns and
        // stores it, exactly like Claude's pre-minted UUID (fresh, not resumed).
        let mut inst = Instance::new("Test", "/tmp/test");
        inst.tool = "opencode".to_string();
        let (sid, is_existing) =
            inst.acquire_session_id_with(&|_| Some("ses_preassigned".to_string()));
        assert_eq!(sid, Some("ses_preassigned".to_string()));
        assert!(!is_existing);
        assert_eq!(inst.agent_session_id, Some("ses_preassigned".to_string()));
    }

    #[test]
    fn opencode_fresh_arm_stays_unowned_when_preassign_fails() {
        // A failed preassign leaves the id unset. AoE must not guess a row from
        // the shared SQLite store.
        let mut inst = Instance::new("Test", "/tmp/test");
        inst.tool = "opencode".to_string();
        let (sid, is_existing) = inst.acquire_session_id_with(&|_| None);
        assert_eq!(sid, None);
        assert!(!is_existing);
        assert_eq!(inst.agent_session_id, None);
    }

    #[test]
    fn non_opencode_fresh_arm_never_calls_preassign_seam() {
        // The seam is opencode-only: Claude mints its own UUID and every other
        // agent starts unpinned, so the seam must not run for them.
        let mut claude = Instance::new("Test", "/tmp/test");
        claude.tool = "claude".to_string();
        let (claude_sid, _) =
            claude.acquire_session_id_with(&|_| panic!("preassign seam ran for claude"));
        assert!(claude_sid.is_some());

        let mut codex = Instance::new("Test", "/tmp/test");
        codex.tool = "codex".to_string();
        let (codex_sid, _) =
            codex.acquire_session_id_with(&|_| panic!("preassign seam ran for codex"));
        assert_eq!(codex_sid, None);
    }

    #[test]
    fn opencode_cleared_intent_also_uses_preassign_seam() {
        // A forced-fresh restart (ResumeIntent::Cleared) is still a new launch,
        // so it preassigns too rather than starting unpinned.
        let mut inst = Instance::new("Test", "/tmp/test");
        inst.tool = "opencode".to_string();
        inst.resume_intent = ResumeIntent::Cleared;
        let (sid, is_existing) = inst.acquire_session_id_with(&|_| Some("ses_cleared".to_string()));
        assert_eq!(sid, Some("ses_cleared".to_string()));
        assert!(!is_existing);
        assert_eq!(inst.agent_session_id, Some("ses_cleared".to_string()));
    }

    #[test]
    fn opencode_preassign_requires_a_mirrorable_launch() {
        let mut inst = Instance::new("Test", "/tmp/test");
        inst.tool = "opencode".to_string();
        assert!(inst.opencode_launch_mirrorable_by_ambient_serve());

        inst.command = "opencode-wrapper".to_string();
        assert!(!inst.opencode_launch_mirrorable_by_ambient_serve());
    }

    #[test]
    #[serial]
    fn opencode_preassign_requires_profile_opt_in() {
        let temp = tempdir().unwrap();
        let _home = EnvGuard::set(&[("HOME", temp.path())]);
        let cases = [
            ("opencode-preassign-off", false),
            ("opencode-preassign-on", true),
        ];

        for (profile, enabled) in cases {
            let config_path = crate::session::get_profile_dir_path(profile)
                .unwrap()
                .join("config.toml");
            std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
            std::fs::write(
                config_path,
                format!(
                    "environment = [\"OPENCODE_CONFIG_DIR=/tmp/opencode-test\"]\n[session]\nopencode_preassign_session_id = {enabled}\n"
                ),
            )
            .unwrap();

            let mut inst = Instance::new("Test", "/tmp/test");
            inst.source_profile = profile.to_string();
            inst.tool = "opencode".to_string();
            assert_eq!(inst.opencode_preassign_enabled(), enabled);
        }
    }

    /// A resume subcommand must land right after the program the pane runs,
    /// and the splice finds that spot by taking the first word. A multi-word
    /// launcher puts something else there, so the flags are dropped rather
    /// than spliced onto the wrong program (#3638).
    #[test]
    fn codex_wrapper_command_never_takes_a_spliced_subcommand() {
        const PROFILE: &str = "codex-wrapper-splice-test";
        let _registry = install_aliases(PROFILE, &[("codex-remote", "codex")]);
        let sid = "11111111-2222-3333-4444-555555555555";

        // The documented custom-agent shape: a multi-token launcher.
        let mut wrapped = Instance::new("wrapper", "/tmp/codex-splice");
        wrapped.source_profile = PROFILE.to_string();
        wrapped.tool = "codex-remote".to_string();
        wrapped.command = "ssh -t lenovo codex".to_string();
        wrapped.agent_session_id = Some(sid.to_string());
        wrapped.resume_intent = ResumeIntent::Use(sid.to_string());
        assert_eq!(
            wrapped.terminal_context_resume_cached(),
            TerminalContextResume::CommandUnsupported
        );
        let mut cmd = wrapped.command.clone();
        assert!(!wrapped.apply_session_flags(&mut cmd, "test").unwrap());
        assert_eq!(
            cmd, "ssh -t lenovo codex",
            "the resume token must not be spliced onto the launcher"
        );

        // An override that does open with the binary keeps its resume.
        let mut direct = Instance::new("direct", "/tmp/codex-splice");
        direct.source_profile = PROFILE.to_string();
        direct.tool = "codex-remote".to_string();
        direct.command = "codex --model o3".to_string();
        direct.agent_session_id = Some(sid.to_string());
        direct.resume_intent = ResumeIntent::Use(sid.to_string());
        assert_eq!(
            direct.terminal_context_resume_cached(),
            TerminalContextResume::Available
        );
        let mut direct_cmd = direct.command.clone();
        assert!(direct.apply_session_flags(&mut direct_cmd, "test").unwrap());
        assert_eq!(direct_cmd, format!("codex resume {sid} --model o3"));

        // A path-qualified script escapes the launch shell's `PATH`, which is
        // the only thing tying a bare token to the agent AoE resolved.
        let mut qualified = Instance::new("qualified", "/tmp/codex-splice");
        qualified.source_profile = PROFILE.to_string();
        qualified.tool = "codex-remote".to_string();
        qualified.command = "/opt/bin/mycodex".to_string();
        qualified.agent_session_id = Some(sid.to_string());
        qualified.resume_intent = ResumeIntent::Use(sid.to_string());
        assert_eq!(
            qualified.terminal_context_resume_cached(),
            TerminalContextResume::CommandUnsupported
        );
        let mut qualified_cmd = qualified.command.clone();
        assert!(!qualified
            .apply_session_flags(&mut qualified_cmd, "test")
            .unwrap());
        assert_eq!(qualified_cmd, "/opt/bin/mycodex");

        // A bare wrapper binary is the program itself, so the splice reaches
        // the agent it execs. This is what `agent_command_override` documents
        // and what a `custom_agents` script is, and dropping it would take
        // resume away from a plain `codex` session that only renamed its
        // binary.
        for command in ["mycodex", "codex-personal"] {
            let mut bare = Instance::new("bare", "/tmp/codex-splice");
            bare.source_profile = PROFILE.to_string();
            bare.tool = "codex-remote".to_string();
            bare.command = command.to_string();
            bare.agent_session_id = Some(sid.to_string());
            bare.resume_intent = ResumeIntent::Use(sid.to_string());
            assert_eq!(
                bare.terminal_context_resume_cached(),
                TerminalContextResume::Available,
                "{command}"
            );
            let mut bare_cmd = bare.command.clone();
            assert!(
                bare.apply_session_flags(&mut bare_cmd, "test").unwrap(),
                "{command}"
            );
            assert_eq!(bare_cmd, format!("{command} resume {sid}"));
        }
    }

    /// A custom agent that wraps a supported one has no `AgentDef` of its
    /// own, so every capture and resume path used to miss on `tool` raw: no
    /// id was pinned at launch and no resume flag was emitted, and each
    /// restart silently started a fresh conversation (#3638).
    #[test]
    fn custom_agent_pins_and_resumes_through_its_detect_as_base() {
        const PROFILE: &str = "custom-agent-resume-test";
        let _registry = install_aliases(
            PROFILE,
            &[
                ("claude-personal", "claude"),
                ("copilot-personal", "copilot"),
                ("droid-personal", "droid"),
            ],
        );

        let mut inst = Instance::new("wrapper", "/tmp/custom-agent-resume");
        inst.source_profile = PROFILE.to_string();
        inst.tool = "claude-personal".to_string();
        inst.command = "claude-personal".to_string();

        let mut fresh = "claude-personal".to_string();
        assert!(!inst.apply_session_flags(&mut fresh, "test").unwrap());
        let sid = inst
            .agent_session_id
            .clone()
            .expect("a wrapper launch must pin the conversation it is about to start");
        assert_eq!(fresh, format!("claude-personal --session-id {sid}"));

        inst.resume_intent = ResumeIntent::Use(sid.clone());
        assert_eq!(
            inst.terminal_context_resume_cached(),
            TerminalContextResume::Available
        );
        let mut restart = "claude-personal".to_string();
        assert!(inst.apply_session_flags(&mut restart, "test").unwrap());
        assert_eq!(restart, format!("claude-personal --resume {sid}"));
        assert_eq!(inst.agent_session_id.as_deref(), Some(sid.as_str()));

        // A wrapper whose base has no verified native resume contract stays
        // silent, pinned id and all.
        let mut unsupported = Instance::new("wrapper", "/tmp/custom-agent-resume");
        unsupported.source_profile = PROFILE.to_string();
        unsupported.tool = "droid-personal".to_string();
        unsupported.command = "droid-personal".to_string();
        unsupported.resume_intent = ResumeIntent::Use(sid.clone());
        assert_eq!(
            unsupported.terminal_context_resume_cached(),
            TerminalContextResume::AgentUnsupported
        );
        let mut droid = "droid-personal".to_string();
        assert!(!unsupported.apply_session_flags(&mut droid, "test").unwrap());
        assert_eq!(droid, "droid-personal");

        // Sandboxing applies the resolved base agent's session-store rule.
        let mut sandboxed = Instance::new("wrapper", "/tmp/custom-agent-resume");
        sandboxed.source_profile = PROFILE.to_string();
        sandboxed.tool = "copilot-personal".to_string();
        sandboxed.command = "copilot-personal".to_string();
        // Default intent: no pin, so copilot's sandbox has nothing to resume.
        sandboxed.agent_session_id = Some(sid);
        sandboxed.sandbox_info = Some(SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test-image".to_string(),
            container_name: "test".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
        });
        assert_eq!(
            sandboxed.terminal_context_resume_cached(),
            TerminalContextResume::SandboxUnsupported
        );
        let mut copilot = "copilot-personal".to_string();
        assert!(!sandboxed.apply_session_flags(&mut copilot, "test").unwrap());
        assert_eq!(copilot, "copilot-personal");
    }
    #[test]
    fn apply_session_flags_returns_acquire_is_existing() {
        let mut inst = Instance::new("Test", "/tmp/test");
        inst.tool = "claude".to_string();
        // Fresh mint (no prior transcript): acquire reports a new session
        // (`--session-id`), so apply_session_flags returns false.
        let mut cmd = String::from("claude");
        assert!(!inst.apply_session_flags(&mut cmd, "test").unwrap());
        // A user-pinned resume intent reports an existing session
        // unconditionally, so apply_session_flags returns true.
        inst.resume_intent = ResumeIntent::Use("019342ab-1234-7def-8901-abcdef012345".to_string());
        let mut cmd2 = String::from("claude");
        assert!(inst.apply_session_flags(&mut cmd2, "test").unwrap());
    }
    #[test]
    fn unsupported_context_without_identity_neither_resumes_nor_polls() {
        let mut inst = Instance::new("unsupported", "/tmp/test");
        inst.tool = "codex".to_string();

        assert_eq!(inst.acquire_session_id_with(&|_| None), (None, false));
        assert_eq!(inst.agent_session_id, None);

        let mut cmd = String::from("codex");
        assert!(!inst.apply_session_flags(&mut cmd, "test").unwrap());
        assert_eq!(cmd, "codex");

        inst.capture_started_at = Some(std::time::SystemTime::now());
        inst.maybe_start_poller_since(None);
        assert!(inst.session_id_poller.is_none());
    }

    #[test]
    #[serial]
    fn resume_intent_use_returns_pinned_sid_without_observation() {
        let mut inst = Instance::new("intent-use", "/tmp/x");
        inst.tool = "claude".to_string();
        inst.agent_session_id = None;
        inst.resume_intent = ResumeIntent::Use("user-pinned".to_string());

        let (sid, is_existing) = inst.acquire_session_id();
        assert_eq!(sid.as_deref(), Some("user-pinned"));
        assert!(is_existing);
        assert_eq!(inst.agent_session_id.as_deref(), Some("user-pinned"));
    }

    #[test]
    #[serial]
    fn resume_intent_use_overrides_observation() {
        let mut inst = Instance::new("intent-use-override", "/tmp/x");
        inst.tool = "claude".to_string();
        inst.agent_session_id = Some("observed".to_string());
        inst.resume_intent = ResumeIntent::Use("user-pinned".to_string());

        let (sid, is_existing) = inst.acquire_session_id();
        assert_eq!(sid.as_deref(), Some("user-pinned"));
        assert!(is_existing);
    }

    #[test]
    #[serial]
    fn resume_intent_cleared_for_claude_generates_fresh_uuid() {
        let mut inst = Instance::new("intent-cleared-claude", "/tmp/x");
        inst.tool = "claude".to_string();
        inst.agent_session_id = Some("observed".to_string());
        inst.resume_intent = ResumeIntent::Cleared;

        let (sid, is_existing) = inst.acquire_session_id();
        assert!(
            sid.is_some(),
            "Claude must always have a session id at launch"
        );
        assert!(!is_existing, "Cleared intent must not report is_existing");
        assert_ne!(sid.as_deref(), Some("observed"));
        assert_eq!(inst.agent_session_id, sid);
    }

    #[test]
    #[serial]
    fn resume_intent_cleared_for_opencode_returns_none() {
        let mut inst = Instance::new("intent-cleared-opencode", "/tmp/x");
        inst.tool = "opencode".to_string();
        inst.agent_session_id = Some("observed".to_string());
        inst.resume_intent = ResumeIntent::Cleared;

        let (sid, is_existing) = inst.acquire_session_id();
        assert_eq!(sid, None);
        assert!(!is_existing);
        assert_eq!(inst.agent_session_id, None);
    }

    #[test]
    #[serial]
    fn resume_intent_default_uses_observed() {
        // Isolate HOME and CLAUDE_CONFIG_DIR at an empty tempdir so
        // `acquire_session_id`'s freshest-observation probe reads scratch
        // state, never the caller's real `~/.claude`. Without this the
        // probe scans `~/.claude/projects/-tmp-x`, and any live transcript
        // there (present in a Claude dev environment) supersedes the stored
        // sid, so the assertion below fails deterministically. Mirrors the
        // `verify_on_resume` submodule's `claude_home_guard`.
        let temp = tempdir().unwrap();
        let mut pairs: Vec<(&'static str, std::path::PathBuf)> =
            vec![("HOME", temp.path().to_path_buf())];
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        pairs.push(("XDG_CONFIG_HOME", temp.path().join(".config")));
        pairs.push(("CLAUDE_CONFIG_DIR", temp.path().join(".claude")));
        let _home = EnvGuard::set(&pairs);

        let mut inst = Instance::new("intent-default", "/tmp/x");
        inst.tool = "claude".to_string();
        inst.agent_session_id = Some("observed".to_string());
        inst.resume_intent = ResumeIntent::Default;

        // Default intent keeps the observed sid as the owned session. With
        // the isolated home holding no transcript for it, the empty thread
        // launches fresh-pinned (`is_existing = false`, `--session-id`)
        // rather than a certain-to-fail `--resume`.
        let (sid, is_existing) = inst.acquire_session_id();
        assert_eq!(sid.as_deref(), Some("observed"));
        assert!(!is_existing);
    }

    #[test]
    fn acquire_default_with_no_observation_generates_uuid_for_claude() {
        let mut inst = Instance::new("acquire-default-fresh", "/tmp/x");
        inst.tool = "claude".to_string();
        inst.agent_session_id = None;
        inst.resume_intent = ResumeIntent::Default;

        let (sid, is_existing) = inst.acquire_session_id();
        assert!(sid.is_some());
        assert!(!is_existing);
        assert_eq!(inst.agent_session_id, sid);
    }

    mod verify_on_resume {
        use super::*;
        use crate::session::capture::encode_claude_project_path;
        use std::fs;
        use std::path::PathBuf;
        use std::time::{Duration, SystemTime};
        use tempfile::{tempdir, TempDir};

        /// Points `HOME`, `CLAUDE_CONFIG_DIR` (and, on Linux/macOS,
        /// `XDG_CONFIG_HOME`) at `temp` for the current test body.
        /// See [`crate::session::test_support`]: the snapshot/restore
        /// is `EnvGuard`'s, so a non-UTF-8 prior value round-trips
        /// instead of being dropped (#2751).
        fn claude_home_guard(temp: &TempDir) -> EnvGuard {
            let mut pairs: Vec<(&'static str, PathBuf)> = vec![("HOME", temp.path().to_path_buf())];
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            pairs.push(("XDG_CONFIG_HOME", temp.path().join(".config")));
            pairs.push(("CLAUDE_CONFIG_DIR", temp.path().join(".claude")));
            EnvGuard::set(&pairs)
        }

        fn write_jsonl_with_mtime(path: &std::path::Path, mtime: SystemTime) {
            fs::write(path, "").unwrap();
            let f = fs::File::options().write(true).open(path).unwrap();
            f.set_times(fs::FileTimes::new().set_modified(mtime))
                .unwrap();
        }

        #[test]
        #[serial]
        fn supersedes_stale_claude_sid_from_pane_sidecar() {
            let temp = tempdir().unwrap();
            let _home = claude_home_guard(&temp);
            let (_hooks, _base, _hook_temp) = crate::hooks::test_support::BaseGuard::ready();

            let project_path = "/tmp/aoe-test-claude-sidecar-rotation";
            let claude_dir = temp
                .path()
                .join(".claude")
                .join("projects")
                .join(encode_claude_project_path(project_path));
            fs::create_dir_all(&claude_dir).unwrap();
            let stale = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
            let fresh = "11111111-2222-3333-4444-555555555555";
            fs::write(claude_dir.join(format!("{fresh}.jsonl")), "").unwrap();

            let mut inst = Instance::new("verify-claude-sidecar-rotation", project_path);
            inst.tool = "claude".to_string();
            inst.agent_session_id = Some(stale.to_string());
            inst.resume_intent = ResumeIntent::Default;
            crate::hooks::write_session_id_via_guard(&inst.id, fresh).unwrap();

            let (sid, is_existing) = inst.acquire_session_id();
            assert_eq!(sid.as_deref(), Some(fresh));
            assert!(is_existing);
            assert_eq!(inst.agent_session_id.as_deref(), Some(fresh));
        }

        #[test]
        #[serial]
        fn observed_sid_without_transcript_downgrades_to_fresh() {
            let temp = tempdir().unwrap();
            let _guard = claude_home_guard(&temp);

            let project_path = "/tmp/aoe-test-observed-no-transcript";
            let claude_dir = temp
                .path()
                .join(".claude")
                .join("projects")
                .join(encode_claude_project_path(project_path));
            fs::create_dir_all(&claude_dir).unwrap();

            let stored = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
            let empty_thread = "11111111-2222-3333-4444-555555555555";
            write_jsonl_with_mtime(
                &claude_dir.join(format!("{stored}.jsonl")),
                SystemTime::now() - Duration::from_secs(120),
            );
            // No .jsonl for `empty_thread`.

            let mut inst = Instance::new("verify-observed-no-transcript", project_path);
            inst.tool = "claude".to_string();
            inst.agent_session_id = Some(stored.to_string());
            inst.resume_intent = ResumeIntent::Default;

            let dir = super::write_sidecar(&inst.id, empty_thread);
            let (sid, is_existing) = inst.acquire_session_id();
            std::fs::remove_dir_all(&dir).ok();

            assert_eq!(sid.as_deref(), Some(empty_thread));
            assert!(
                !is_existing,
                "an observed sid with no transcript must launch as \
                 --session-id, never --resume"
            );
        }

        // An empty Claude thread killed before its first prompt has a
        // stored sid but no transcript on disk. `claude --resume <sid>`
        // would fail for it every time (the "resume failed for sid ...;
        // preserved for explicit retry" loop), so acquire must launch it as
        // a fresh pinned session (`--session-id <sid>`, is_existing=false)
        // while keeping the id stable for a later first prompt.
        #[test]
        #[serial]
        fn stored_sid_without_transcript_launches_fresh_pinned() {
            let temp = tempdir().unwrap();
            let _guard = claude_home_guard(&temp);

            let project_path = "/tmp/aoe-test-2291-no-jsonl";
            let stored = "12121212-3434-5656-7878-9a9a9a9a9a9a";

            let mut inst = Instance::new("verify-claude-no-jsonl", project_path);
            inst.tool = "claude".to_string();
            inst.agent_session_id = Some(stored.to_string());
            inst.resume_intent = ResumeIntent::Default;

            let (sid, is_existing) = inst.acquire_session_id();
            assert_eq!(sid.as_deref(), Some(stored));
            assert!(
                !is_existing,
                "a stored sid with no transcript must launch fresh-pinned, not --resume"
            );
            assert_eq!(inst.agent_session_id.as_deref(), Some(stored));
        }

        // Transcript age is irrelevant once the stored id is owned. Existence
        // proves the native conversation can still be resumed.
        #[test]
        #[serial]
        fn stored_sid_with_stale_transcript_still_resumes() {
            let temp = tempdir().unwrap();
            let _guard = claude_home_guard(&temp);

            let project_path = "/tmp/aoe-test-stale-transcript";
            let claude_dir = temp
                .path()
                .join(".claude")
                .join("projects")
                .join(encode_claude_project_path(project_path));
            fs::create_dir_all(&claude_dir).unwrap();

            let stored = "12121212-3434-5656-7878-9a9a9a9a9a9a";
            write_jsonl_with_mtime(
                &claude_dir.join(format!("{stored}.jsonl")),
                SystemTime::now() - Duration::from_secs(3600),
            );

            let mut inst = Instance::new("verify-claude-stale", project_path);
            inst.tool = "claude".to_string();
            inst.agent_session_id = Some(stored.to_string());
            inst.resume_intent = ResumeIntent::Default;

            let (sid, is_existing) = inst.acquire_session_id();
            assert_eq!(sid.as_deref(), Some(stored));
            assert!(
                is_existing,
                "a real (if idle) transcript on disk must resume with --resume"
            );
            assert_eq!(inst.agent_session_id.as_deref(), Some(stored));
        }

        /// #3399: two sessions share a `project_path` but sit in profiles
        /// pinned to different `CLAUDE_CONFIG_DIR`s. Each must resume its
        /// own conversation. Resolving the default `~/.claude` instead
        /// reports both transcripts absent and downgrades every launch to
        /// `--session-id <sid>`, which the agent rejects as already in use.
        #[test]
        #[serial]
        fn same_cwd_sessions_resume_their_own_profile_scoped_conversation() {
            let temp = tempdir().unwrap();
            let _guard = claude_home_guard(&temp);

            let project_path = "/tmp/aoe-test-3399-shared-cwd";
            let cases = [
                ("aoe-3399-personal", "11111111-1111-4111-8111-111111111111"),
                ("aoe-3399-work", "22222222-2222-4222-8222-222222222222"),
            ];
            for (profile, sid) in cases {
                let claude_home = temp.path().join(format!(".claude-{profile}"));
                let dir = claude_home
                    .join("projects")
                    .join(encode_claude_project_path(project_path));
                fs::create_dir_all(&dir).unwrap();
                // The profile-scoped transcript, not another config tree,
                // proves this stored id is resumable.
                write_jsonl_with_mtime(
                    &dir.join(format!("{sid}.jsonl")),
                    SystemTime::now() - Duration::from_secs(3600),
                );

                let config_path = crate::session::get_profile_dir_path(profile)
                    .unwrap()
                    .join("config.toml");
                fs::create_dir_all(config_path.parent().unwrap()).unwrap();
                fs::write(
                    &config_path,
                    format!(
                        "environment = [\"CLAUDE_CONFIG_DIR={}\"]\n",
                        claude_home.display()
                    ),
                )
                .unwrap();
            }

            for (profile, sid) in cases {
                let mut inst = Instance::new(profile, project_path);
                inst.source_profile = profile.to_string();
                inst.tool = "claude".to_string();
                inst.agent_session_id = Some(sid.to_string());
                inst.resume_intent = ResumeIntent::Default;

                let (acquired, is_existing) = inst.acquire_session_id();
                assert_eq!(acquired.as_deref(), Some(sid));
                assert!(
                    is_existing,
                    "{profile}: transcript under the profile's own CLAUDE_CONFIG_DIR \
                     must resume with --resume, not launch fresh-pinned"
                );
            }

            // A `before_session` hook minting CLAUDE_CONFIG_DIR is the
            // documented account-switcher pattern, and its value wins over
            // the profile's on the launched pane. Reading the shadowed
            // profile value here would resolve a config dir the agent
            // never opens, reintroducing the same downgrade.
            let (shadowed_profile, other_sid) = (cases[0].0, cases[1].1);
            let mut inst = Instance::new("minted-switcher", project_path);
            inst.source_profile = shadowed_profile.to_string();
            inst.tool = "claude".to_string();
            inst.agent_session_id = Some(other_sid.to_string());
            inst.resume_intent = ResumeIntent::Default;
            inst.pending_host_env = vec![(
                "CLAUDE_CONFIG_DIR".to_string(),
                temp.path()
                    .join(format!(".claude-{}", cases[1].0))
                    .to_string_lossy()
                    .into_owned(),
            )];

            let (acquired, is_existing) = inst.acquire_session_id();
            assert_eq!(acquired.as_deref(), Some(other_sid));
            assert!(
                is_existing,
                "a before_session-minted CLAUDE_CONFIG_DIR must win over the \
                 profile's, matching what the launch injects into the pane"
            );
        }

        #[test]
        #[serial]
        fn unaffected_for_unsupported_tool() {
            let temp = tempdir().unwrap();
            let _guard = claude_home_guard(&temp);

            let mut inst = Instance::new("verify-cursor", "/tmp/aoe-test-2291-cursor");
            inst.tool = "cursor".to_string();
            inst.agent_session_id = Some("stored-cursor-sid".to_string());
            inst.resume_intent = ResumeIntent::Default;

            let (sid, is_existing) = inst.acquire_session_id();
            assert_eq!(sid.as_deref(), Some("stored-cursor-sid"));
            assert!(is_existing);
            assert_eq!(inst.agent_session_id.as_deref(), Some("stored-cursor-sid"));
        }

        // A shared cwd may contain a peer's newer transcript. Transcript order
        // is not identity authority: the instance-keyed sidecar must select this
        // pane's conversation while the unrelated peer artifact is ignored.
        #[test]
        #[serial]
        fn sidecar_wins_over_fresher_peer_jsonl() {
            let temp = tempdir().unwrap();
            let _guard = claude_home_guard(&temp);

            let project_path = "/tmp/aoe-test-2344-shared-cwd";
            let claude_dir = temp
                .path()
                .join(".claude")
                .join("projects")
                .join(encode_claude_project_path(project_path));
            fs::create_dir_all(&claude_dir).unwrap();

            // `mine` is this instance's real conversation (named by its
            // sidecar). `peer` is a co-located peer's conversation that is
            // strictly freshest on disk. `stored` is a stale id distinct
            // from `mine`, so asserting `sid == mine` proves the sidecar
            // actively overrode the stored value rather than the stored
            // value passing through unchanged.
            let mine = "aaaaaaaa-1111-4111-8111-aaaaaaaaaaaa";
            let peer = "bbbbbbbb-2222-4222-8222-bbbbbbbbbbbb";
            let stored = "cccccccc-3333-4333-8333-cccccccccccc";
            let now = SystemTime::now();
            write_jsonl_with_mtime(
                &claude_dir.join(format!("{mine}.jsonl")),
                now - Duration::from_secs(120),
            );
            write_jsonl_with_mtime(
                &claude_dir.join(format!("{peer}.jsonl")),
                now - Duration::from_secs(5),
            );

            let mut inst = Instance::new("verify-2344-shared-cwd", project_path);
            inst.tool = "claude".to_string();
            inst.agent_session_id = Some(stored.to_string());
            inst.resume_intent = ResumeIntent::Default;

            let dir = super::write_sidecar(&inst.id, mine);
            let (sid, is_existing) = inst.acquire_session_id();
            std::fs::remove_dir_all(&dir).ok();

            // The authoritative sidecar overrides the stale stored sid;
            // the peer's fresher jsonl never wins.
            assert_eq!(sid.as_deref(), Some(mine));
            assert!(is_existing);
            assert_eq!(inst.agent_session_id.as_deref(), Some(mine));
        }

        #[test]
        #[serial]
        fn idle_sidecar_still_overrides_stored_identity() {
            let temp = tempdir().unwrap();
            let _guard = claude_home_guard(&temp);
            let mut inst = Instance::new("idle-sidecar", "/tmp/idle-sidecar");
            inst.tool = "claude".to_string();
            inst.agent_session_id = Some("stored-old".to_string());
            inst.resume_intent = ResumeIntent::Default;

            let dir = super::write_sidecar(&inst.id, "published-new");
            let stale = SystemTime::now() - Duration::from_secs(10 * 60);
            std::fs::File::options()
                .write(true)
                .open(dir.join("session_id"))
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(stale))
                .unwrap();

            assert_eq!(
                inst.capture_freshest_session_id().as_deref(),
                Some("published-new")
            );
            std::fs::remove_dir_all(dir).ok();
        }

        // Sandboxed Claude uses the same instance-keyed host sidecar through
        // its hook bind mount. Shared container transcripts are not consulted as
        // a competing identity source.
        #[test]
        #[serial]
        fn sidecar_consulted_for_sandboxed_claude() {
            let temp = tempdir().unwrap();
            let _guard = claude_home_guard(&temp);

            let project_path = "/tmp/aoe-test-2344-sandbox";
            let claude_dir = temp
                .path()
                .join(".claude")
                .join("projects")
                .join(encode_claude_project_path(project_path));
            fs::create_dir_all(&claude_dir).unwrap();

            // `stored` is distinct from the sidecar `mine`, so the assertion
            // proves the sidecar actively overrode the stale stored value.
            let mine = "eeeeeeee-5555-4555-8555-eeeeeeeeeeee";
            let peer = "ffffffff-6666-4666-8666-ffffffffffff";
            let stored = "dddddddd-7777-4777-8777-dddddddddddd";
            let now = SystemTime::now();
            write_jsonl_with_mtime(
                &claude_dir.join(format!("{mine}.jsonl")),
                now - Duration::from_secs(120),
            );
            write_jsonl_with_mtime(
                &claude_dir.join(format!("{peer}.jsonl")),
                now - Duration::from_secs(5),
            );

            let mut inst = Instance::new("verify-2344-sandbox", project_path);
            inst.tool = "claude".to_string();
            inst.agent_session_id = Some(stored.to_string());
            inst.resume_intent = ResumeIntent::Default;
            inst.sandbox_info = Some(crate::session::SandboxInfo {
                enabled: true,
                container_id: None,
                image: "test-image".to_string(),
                container_name: "verify-2344-sandbox".to_string(),
                extra_env: None,
                custom_instruction: None,
                before_start_env: Vec::new(),
                container_workdir: None,
            });
            assert!(inst.is_sandboxed());

            let dir = super::write_sidecar(&inst.id, mine);
            let (sid, is_existing) = inst.acquire_session_id();
            std::fs::remove_dir_all(&dir).ok();

            // The host-readable sidecar names this sandbox pane's conversation;
            // the peer transcript is never considered.
            assert_eq!(sid.as_deref(), Some(mine));
            assert!(is_existing);
            assert_eq!(inst.agent_session_id.as_deref(), Some(mine));
        }

        // A pinnable Pi launch owns its native id before the pane starts.
        #[test]
        fn pi_fresh_launch_pins_the_minted_id() {
            let pinned = "aaaaaaaa-1111-4111-8111-aaaaaaaaaaaa";

            let mut inst = Instance::new("pi-fresh", "/tmp/pi-fresh");
            inst.tool = "pi".to_string();
            let (sid, is_existing) = inst.acquire_session_id_with(&|_| Some(pinned.to_string()));
            assert_eq!(sid.as_deref(), Some(pinned));
            assert!(
                !is_existing,
                "a pinned launch is a new session, not a resume"
            );
            assert_eq!(inst.agent_session_id.as_deref(), Some(pinned));
            assert_eq!(
                crate::session::instance::launch_command::build_resume_flags(
                    "pi",
                    pinned,
                    is_existing
                ),
                format!("--session-id {pinned}")
            );

            let mut unpinnable = Instance::new("pi-unpinnable", "/tmp/pi-fresh");
            unpinnable.tool = "pi".to_string();
            assert_eq!(unpinnable.acquire_session_id_with(&|_| None), (None, false));
            assert_eq!(unpinnable.agent_session_id, None);
        }
    }
    #[test]
    fn external_session_selectors_skip_aoe_injection_without_managed_state() {
        for command in [
            "claude --resume external",
            "claude --resume=external",
            "claude --session-id=external",
            "claude -c",
            "claude --continue",
            "claude -r external",
            "claude --from-pr=3678",
            "claude --teleport external",
            "claude --cloud external",
            "claude --remote external",
            "claude --fork-session",
        ] {
            let mut inst = Instance::new("claude", "/tmp/x");
            inst.tool = "claude".to_string();
            let mut actual = command.to_string();
            assert!(!inst.apply_session_flags(&mut actual, "test").unwrap());
            assert_eq!(actual, command);
            assert!(inst.agent_session_id.is_none());
        }
    }

    #[test]
    fn external_session_selector_conflicts_with_managed_state() {
        let sid = "11111111-2222-3333-4444-555555555555";
        for (stored, intent) in [
            (Some(sid.to_string()), ResumeIntent::Default),
            (None, ResumeIntent::Use(sid.to_string())),
            (
                Some("child-session".to_string()),
                ResumeIntent::Fork {
                    from: sid.to_string(),
                },
            ),
        ] {
            let mut inst = Instance::new("claude", "/tmp/x");
            inst.tool = "claude".to_string();
            inst.agent_session_id = stored;
            inst.resume_intent = intent;
            let error = inst
                .apply_session_flags(&mut "claude --resume external".to_string(), "test")
                .unwrap_err();
            assert!(error
                .to_string()
                .contains("already contains native session selector"));
            assert!(error
                .to_string()
                .contains("clear the AoE-managed resume state"));
        }
    }

    #[test]
    fn alternate_claude_selectors_conflict_with_managed_state() {
        let sid = "11111111-2222-3333-4444-555555555555";
        for command in [
            "claude -c",
            "claude --continue",
            "claude -r external",
            "claude --from-pr 3678",
            "claude --teleport external",
            "claude --cloud external",
            "claude --remote external",
            "claude --fork-session",
        ] {
            let mut inst = Instance::new("claude", "/tmp/x");
            inst.tool = "claude".to_string();
            inst.agent_session_id = Some(sid.to_string());
            let error = inst
                .apply_session_flags(&mut command.to_string(), "test")
                .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("already contains native session selector"),
                "command={command:?}, error={error:#}"
            );
        }
    }

    #[test]
    fn codex_selector_requires_subcommand_position_and_resolves_alias() {
        let mut external = Instance::new("codex", "/tmp/x");
        external.tool = "codex".to_string();
        let mut command = "codex resume external".to_string();
        assert!(!external.apply_session_flags(&mut command, "test").unwrap());
        assert_eq!(command, "codex resume external");

        let sid = "11111111-2222-3333-4444-555555555555";
        let mut value_token = Instance::new("codex", "/tmp/x");
        value_token.tool = "codex".to_string();
        value_token.resume_intent = ResumeIntent::Use(sid.to_string());
        let mut command = "codex --model resume".to_string();
        assert!(value_token
            .apply_session_flags(&mut command, "test")
            .unwrap());
        assert_eq!(command, format!("codex resume {sid} --model resume"));

        const PROFILE: &str = "selector-detect-as-alias";
        let _registry = install_aliases(PROFILE, &[("work-claude", "claude")]);
        let mut alias = Instance::new("alias", "/tmp/x");
        alias.source_profile = PROFILE.to_string();
        alias.tool = "work-claude".to_string();
        alias.command = "claude --resume external".to_string();
        let mut command = alias.command.clone();
        assert!(!alias.apply_session_flags(&mut command, "test").unwrap());
        assert!(alias.agent_session_id.is_none());
    }

    #[test]
    fn terminal_context_resume_matches_emission_constraints() {
        let sid = "44444444-4444-4444-8444-444444444444".to_string();
        let mut inst = Instance::new("context", "/tmp/context");
        inst.tool = "claude".to_string();
        assert_eq!(
            inst.terminal_context_resume_with_runtime_source(|| {
                crate::tmux::SessionExistence::Absent
            }),
            TerminalContextResume::NoTarget
        );
        assert_eq!(
            inst.terminal_context_resume_with_runtime_source(|| {
                crate::tmux::SessionExistence::Present
            }),
            TerminalContextResume::RuntimeCheckRequired
        );
        assert_eq!(
            inst.terminal_context_resume_with_runtime_source(|| {
                crate::tmux::SessionExistence::Unknown
            }),
            TerminalContextResume::RuntimeCheckRequired
        );
        inst.agent_session_id = Some(sid.clone());
        assert_eq!(
            inst.terminal_context_resume_cached(),
            TerminalContextResume::RuntimeCheckRequired
        );
        inst.resume_intent = ResumeIntent::Use(sid.clone());
        assert_eq!(
            inst.terminal_context_resume_cached(),
            TerminalContextResume::Available
        );
        inst.resume_probe_failed_sid = Some(sid.clone());
        assert_eq!(
            inst.terminal_context_resume_cached(),
            TerminalContextResume::Available
        );
        inst.resume_intent = ResumeIntent::Default;
        assert_eq!(
            inst.terminal_context_resume_cached(),
            TerminalContextResume::PreviousFailure
        );
        inst.resume_intent = ResumeIntent::Use("bad target".to_string());
        assert_eq!(
            inst.terminal_context_resume_cached(),
            TerminalContextResume::InvalidTarget
        );
        let mut command = "claude".to_string();
        assert!(!inst.apply_session_flags(&mut command, "test").unwrap());
        assert_eq!(command, "claude");
        inst.resume_intent = ResumeIntent::Cleared;
        assert_eq!(
            inst.terminal_context_resume_cached(),
            TerminalContextResume::ForcedFresh
        );
        inst.resume_intent = ResumeIntent::Fork { from: sid.clone() };
        assert_eq!(
            inst.terminal_context_resume_cached(),
            TerminalContextResume::ForkPending
        );

        inst.tool = "droid".to_string();
        assert_eq!(
            inst.terminal_context_resume_cached(),
            TerminalContextResume::AgentUnsupported
        );

        let sandbox = Some(SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test-image".to_string(),
            container_name: "test".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
        });

        // Copilot publishes no identity in any environment, so an
        // automatically stored host id must not cross into a container.
        inst.tool = "copilot".to_string();
        inst.sandbox_info = sandbox.clone();
        inst.resume_intent = ResumeIntent::Default;
        for target in [None, Some(sid.clone())] {
            inst.agent_session_id = target;
            assert_eq!(
                inst.terminal_context_resume_cached(),
                TerminalContextResume::SandboxUnsupported,
                "an automatic copilot id must start fresh in a sandbox"
            );
        }
        // An explicit pin names a conversation, so it stays authoritative
        // against this instance's own store.
        inst.resume_intent = ResumeIntent::Use(sid.clone());
        assert_eq!(
            inst.terminal_context_resume_cached(),
            TerminalContextResume::Available,
            "a pinned copilot conversation must still be attempted"
        );
        let mut pinned = "copilot".to_string();
        assert!(inst.apply_session_flags(&mut pinned, "test").unwrap());
        assert_eq!(pinned, format!("copilot --session-id {sid}"));

        // kimi and prime-agent resume from the per-instance sandbox store
        // v027 stages for them, so the sandbox no longer suppresses them.
        for tool in ["kimi", "prime-agent"] {
            inst.tool = tool.to_string();
            inst.sandbox_info = sandbox.clone();
            inst.resume_intent = ResumeIntent::Use(sid.clone());
            inst.agent_session_id = Some(sid.clone());
            assert_eq!(
                inst.terminal_context_resume_cached(),
                TerminalContextResume::Available,
                "{tool} has a private sandbox store to resume from"
            );
        }
    }
}
