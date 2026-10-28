//! Prime Agent capture: launch options, session directory plan, root publication.

use super::*;
use crate::agents::SessionCaptureBackend;
use crate::session::config::container_config::PRIME_AGENT_DIR_IN_CONTAINER;

const PRIME_AGENT_HEADER_MAX_BYTES: u64 = 64 * 1024;
const PRIME_SETTINGS_PRESENCE: &str = r#"p=$1
while [ ! -e "$p" ] && [ ! -L "$p" ]; do
    [ "$p" != / ] || exit 1
    p=${p%/*}; [ -n "$p" ] || p=/
done
if [ "$p" = "$1" ]; then
    [ ! -L "$p" ] && [ -f "$p" ] && [ -r "$p" ] || exit 1
    printf present
else
    [ -d "$p" ] && [ -x "$p" ] || exit 1
    printf absent
fi"#;

fn settings_from_native_archive(
    bytes: &[u8],
) -> anyhow::Result<Option<serde_json::Map<String, serde_json::Value>>> {
    let mut archive = tar::Archive::new(bytes);
    let mut entries = archive.entries()?;
    let entry = entries
        .next()
        .transpose()?
        .context("native settings archive is empty")?;
    anyhow::ensure!(
        entry.header().entry_type().is_file(),
        "native settings are not a regular file"
    );
    anyhow::ensure!(
        entry.path()?.as_ref() == Path::new("settings.json"),
        "native settings archive names another file"
    );
    anyhow::ensure!(
        entry.size() <= 64 * 1024,
        "native settings exceed the size limit"
    );
    let start = usize::try_from(entry.raw_file_position())?;
    let end = start
        .checked_add(usize::try_from(entry.size())?)
        .context("native settings archive offset overflow")?;
    let data = bytes
        .get(start..end)
        .context("native settings archive is truncated")?;
    let settings = serde_json::from_slice(data).ok();
    drop(entry);
    anyhow::ensure!(
        entries.next().transpose()?.is_none(),
        "native settings archive contains another entry"
    );
    Ok(settings)
}

fn read_prime_native_settings(
    inputs: &super::execution::NativeLaunchInputs,
    path: &Path,
) -> anyhow::Result<Option<serde_json::Map<String, serde_json::Value>>> {
    let Some(container) = &inputs.container else {
        return super::session_id::read_session_settings(path);
    };
    anyhow::ensure!(
        container.host_path(path, false).is_some(),
        "Prime settings have no readable host projection"
    );
    let path = path.to_str().context("Prime settings path is not UTF-8")?;
    let args = [
        "/bin/sh",
        "-c",
        PRIME_SETTINGS_PRESENCE,
        "aoe-settings",
        path,
    ]
    .map(str::to_owned);
    let presence = crate::session::capture::run_with_timeout_limit(
        container.runtime.exec(&container.id, "/", &args),
        std::time::Duration::from_secs(2),
        "native Prime settings presence",
        1024,
    )
    .context("cannot observe native Prime settings")?;
    if presence == b"absent" {
        return Ok(None);
    }
    anyhow::ensure!(
        presence == b"present",
        "native Prime settings presence is unavailable"
    );
    // Native Prime writes private settings as the container user, not the host user.
    let command =
        container
            .runtime
            .command(&["cp".into(), format!("{}:{path}", container.id), "-".into()]);
    let output = crate::session::capture::run_with_timeout_limit(
        command,
        std::time::Duration::from_secs(2),
        "native Prime settings archive",
        128 * 1024,
    )
    .context("cannot read native Prime settings")?;
    settings_from_native_archive(&output)
}

#[derive(Default)]
pub(super) struct PrimeAgentLaunchOptions {
    cwd: Option<String>,
    session_dir: Option<String>,
    mode: Option<String>,
    no_session: bool,
    print: bool,
}

impl PrimeAgentLaunchOptions {
    pub(super) fn is_print(&self) -> bool {
        self.print || self.mode.as_deref() == Some("json")
    }
}

pub(super) const PRIME_VALUE_OPTIONS: &[&str] = &[
    "--model",
    "--provider",
    "--api-key",
    "--models",
    "--thinking",
    "--system-prompt",
    "--append-system-prompt",
    "--goal",
    "--goal-token-budget",
    "--autonomous-gate",
    "--autonomous-gate-retries",
    "--autonomous-gate-timeout-ms",
    "--autonomous-max-continuations",
    "--autonomous-max-turns",
    "--autonomous-max-tokens",
    "--autonomous-timeout-ms",
    "--skill",
    "--prompt-template",
    "--theme",
    "--session-dir",
    "--cwd",
    "--mode",
];

fn parse_prime_agent_launch_options(words: &[String]) -> Option<PrimeAgentLaunchOptions> {
    let mut options = PrimeAgentLaunchOptions::default();
    let mut index = 1;
    while let Some(argument) = words.get(index).map(String::as_str) {
        match argument {
            "--" => break,
            "--print" | "-p" => options.print = true,
            "--no-session" => options.no_session = true,
            "--mode" | "--cwd" | "--session-dir" => {
                let value = words.get(index + 1)?.clone();
                match argument {
                    "--cwd" => options.cwd = Some(value),
                    "--session-dir" => options.session_dir = Some(value),
                    _ if matches!(value.as_str(), "text" | "json" | "rpc" | "acp" | "daemon") => {
                        options.mode = Some(value)
                    }
                    _ => return None,
                }
                index += 1;
            }
            _ if PRIME_VALUE_OPTIONS.contains(&argument)
                || matches!(argument, "--daemon-socket" | "--fork") =>
            {
                words.get(index + 1)?;
                index += 1;
            }
            _ => {}
        }
        index += 1;
    }
    Some(options)
}

fn resolve_prime_agent_path(value: &str, cwd: &Path, home: &Path) -> PathBuf {
    let expanded = match value.strip_prefix('~') {
        Some("") => home.to_path_buf(),
        Some(rest) if rest.starts_with('/') => home.join(&rest[1..]),
        _ => PathBuf::from(value),
    };
    crate::git::template::lexical_normalize(&cwd.join(expanded))
}

fn validated_prime_root_publication(
    plan: &PrimeAgentCapturePlan,
    publication: &crate::process::prime::PrimeObservation,
) -> Option<PrimeRootPublication> {
    use std::io::{BufRead as _, Read as _};

    let id = validated_session_id(publication.session_id.clone())?;
    let expected_cwd = crate::session::capture::canonicalize_or_raw(&plan.container_cwd);
    if crate::session::capture::canonicalize_or_raw(&publication.cwd) != expected_cwd {
        return None;
    }
    let Some(path) = &publication.session_file else {
        return Some(PrimeRootPublication::Pending(id));
    };
    if !path.is_absolute()
        || crate::git::template::lexical_normalize(path) != *path
        || path.parent()? != plan.container_session_dir
        || path.extension()?.to_str()? != "jsonl"
    {
        return None;
    }
    let root = crate::session::AnchoredDir::open(&plan.store).ok()?;
    let relative = plan.session_dir.join(path.file_name()?);
    let Some(file) = root.open_regular(&relative, usize::MAX).ok()? else {
        // Only a missing leaf under readable, anchored parents establishes emptiness.
        return root
            .regular_lookup(&relative)
            .ok()?
            .is_none()
            .then_some(PrimeRootPublication::Pending(id));
    };
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
            .is_some_and(|cwd| crate::session::capture::canonicalize_or_raw(cwd) == expected_cwd);
    valid.then_some(PrimeRootPublication::Ready(id))
}

impl Instance {
    pub(super) fn prime_agent_capture_options(&self) -> Option<PrimeAgentLaunchOptions> {
        if self.resolved_capture_backend() != Some(SessionCaptureBackend::PrimeAgent)
            || !self.is_sandboxed()
        {
            return None;
        }
        let agent = self.resolved_agent()?;
        if !self.launch_invokes_resolved_agent_directly(agent) {
            return None;
        }
        self.prime_agent_launch_options()
    }

    pub(super) fn prime_agent_launch_options(&self) -> Option<PrimeAgentLaunchOptions> {
        let parsed = parse_launch_command(self.get_tool_command())?;
        let mut words = parsed.words;
        words.extend(shell_words::split(&self.extra_args).ok()?);
        let options = parse_prime_agent_launch_options(&words)?;
        (!options.no_session && !matches!(options.mode.as_deref(), Some("daemon" | "rpc" | "acp")))
            .then_some(options)
    }

    #[cfg(test)]
    pub(super) fn prime_agent_capture_plan(
        &self,
        options: PrimeAgentLaunchOptions,
    ) -> anyhow::Result<PrimeAgentCapturePlan> {
        let file_watch = crate::file_watch::FileWatchService::noop();
        self.prime_agent_capture_plan_in(options, CaptureStorage::Profiles(&file_watch))
    }

    pub(super) fn prime_agent_capture_plan_in(
        &self,
        options: PrimeAgentLaunchOptions,
        stores: CaptureStorage<'_>,
    ) -> anyhow::Result<PrimeAgentCapturePlan> {
        let store = self
            .sandbox_capture_store_dir()
            .context("managed Prime store is unavailable")?;
        let launch_config = stores.launch_configuration(
            &self.effective_profile(),
            std::path::Path::new(&self.project_path),
        )?;
        let config = self
            .build_container_config(&launch_config)
            .context("cannot build Prime container configuration")?;
        self.resolve_prime_agent_capture_plan(&config, store, options)
    }

    #[cfg(test)]
    pub(super) fn prime_agent_capture_plan_with(
        &self,
        config: &crate::containers::ContainerConfig,
        store: PathBuf,
    ) -> Option<PrimeAgentCapturePlan> {
        let options = self.prime_agent_capture_options()?;
        self.resolve_prime_agent_capture_plan(config, store, options)
            .inspect_err(|error| {
                tracing::debug!(target: "session.capture", session = %self.id, reason = %format_args!("{error:#}"),
                    "Prime native capture unavailable");
            })
            .ok()
    }

    fn resolve_prime_agent_capture_plan(
        &self,
        config: &crate::containers::ContainerConfig,
        store: PathBuf,
        options: PrimeAgentLaunchOptions,
    ) -> anyhow::Result<PrimeAgentCapturePlan> {
        let environment_value = |key: &str| {
            config
                .environment
                .iter()
                .find(|entry| entry.key() == key)
                .map(|entry| entry.value())
        };
        anyhow::ensure!(
            config.uses_default_container_home(),
            "Prime capture requires the default container HOME"
        );
        Self::resolve_prime_agent_layout(
            store,
            options,
            Path::new(&self.container_workdir()),
            (Path::new("/root"), Path::new(PRIME_AGENT_DIR_IN_CONTAINER)),
            environment_value("PRIME_AGENT_SESSION_DIR")
                .or_else(|| environment_value("PRIME_AGENT_CODING_AGENT_SESSION_DIR")),
            |path, writable| config.host_path_for_container_path(path, writable),
            |path| {
                super::session_id::read_session_settings(
                    &config
                        .host_path_for_container_path(path, false)
                        .context("Prime settings are not mapped to a readable host path")?,
                )
            },
        )
    }

    pub(super) fn prime_agent_launch_plan_from_inputs(
        &self,
        inputs: &super::execution::NativeLaunchInputs,
        agent_dir: &Path,
        home: &Path,
    ) -> anyhow::Result<PrimeAgentCapturePlan> {
        let root = inputs.canonical_path(agent_dir)?;
        let store = if let Some(container) = &inputs.container {
            anyhow::ensure!(
                home == Path::new("/root"),
                "Prime capture requires the default container HOME"
            );
            container
                .host_path(&root, true)
                .context("Prime store is not mounted from a writable local filesystem")?
        } else {
            root.clone()
        };
        let mut options = self
            .prime_agent_launch_options()
            .context("Prime launch options do not support a managed conversation")?;
        if let Some(cwd) = options.cwd.as_deref() {
            let cwd = resolve_prime_agent_path(cwd, &inputs.cwd, home);
            options.cwd = Some(
                inputs
                    .canonical_path(&cwd)?
                    .to_str()
                    .context("Prime working directory is not UTF-8")?
                    .to_owned(),
            );
        }
        Self::resolve_prime_agent_layout(
            store,
            options,
            &inputs.cwd,
            (home, &root),
            inputs
                .environment
                .get("PRIME_AGENT_SESSION_DIR")
                .or_else(|| {
                    inputs
                        .environment
                        .get("PRIME_AGENT_CODING_AGENT_SESSION_DIR")
                })
                .map(String::as_str),
            |path, writable| {
                let path = inputs.canonical_path(path).ok()?;
                match &inputs.container {
                    Some(container) => container.host_path(&path, writable),
                    None => Some(path),
                }
            },
            |path| read_prime_native_settings(inputs, path),
        )
    }

    fn resolve_prime_agent_layout(
        store: PathBuf,
        options: PrimeAgentLaunchOptions,
        launch_cwd: &Path,
        (home, agent_dir): (&Path, &Path),
        environment_session_dir: Option<&str>,
        host_path_for: impl Fn(&Path, bool) -> Option<PathBuf>,
        read_settings: impl Fn(
            &Path,
        )
            -> anyhow::Result<Option<serde_json::Map<String, serde_json::Value>>>,
    ) -> anyhow::Result<PrimeAgentCapturePlan> {
        let container_cwd = options.cwd.as_deref().map_or_else(
            || launch_cwd.to_path_buf(),
            |cwd| resolve_prime_agent_path(cwd, launch_cwd, home),
        );
        anyhow::ensure!(
            container_cwd.is_absolute(),
            "Prime working directory is not absolute"
        );
        let configured_session_dir = options
            .session_dir
            .filter(|value| !value.is_empty())
            .or_else(|| {
                environment_session_dir
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned)
            });
        let session_dir_value = if let Some(value) = configured_session_dir {
            value
        } else {
            let global_container_path = agent_dir.join("settings.json");
            let project_container_path = container_cwd.join(".prime/agent/settings.json");
            let global = read_settings(&global_container_path).with_context(|| {
                format!(
                    "cannot safely read Prime settings {}",
                    global_container_path.display()
                )
            })?;
            let project = read_settings(&project_container_path).with_context(|| {
                format!(
                    "cannot safely read Prime settings {}",
                    project_container_path.display()
                )
            })?;
            match project
                .as_ref()
                .and_then(|settings| settings.get("sessionDir"))
                .or_else(|| {
                    global
                        .as_ref()
                        .and_then(|settings| settings.get("sessionDir"))
                }) {
                Some(serde_json::Value::String(value)) => value.clone(),
                Some(serde_json::Value::Null) | None => {
                    format!(
                        "{}/sessions",
                        agent_dir.to_str().context("Prime store is not UTF-8")?
                    )
                }
                Some(_) => anyhow::bail!("Prime sessionDir setting is neither a string nor null"),
            }
        };
        let container_session_dir =
            resolve_prime_agent_path(&session_dir_value, &container_cwd, home);
        let session_dir = container_session_dir
            .strip_prefix(agent_dir)
            .context("Prime session directory is outside the managed store")?
            .to_path_buf();
        let mapped = host_path_for(&container_session_dir, true)
            .context("Prime session directory is not mapped to a writable host path")?;
        anyhow::ensure!(
            mapped == store.join(&session_dir),
            "Prime session directory is shadowed by another mount"
        );
        Ok(PrimeAgentCapturePlan {
            store,
            session_dir,
            container_session_dir,
            container_cwd: container_cwd
                .to_str()
                .context("Prime working directory is not UTF-8")?
                .to_string(),
        })
    }

    pub(super) fn prime_root_publication(&self, fresh: bool) -> Option<PrimeRootPublication> {
        let active = self.active_execution.as_ref()?;
        let Some(CaptureContext::Prime {
            plan,
            runtime: Some(runtime),
        }) = &active.capture
        else {
            return None;
        };
        validated_prime_root_publication(plan, runtime.observe(fresh)?.as_ref())
    }

    pub(super) fn prime_root_observation(
        &self,
        sid: String,
    ) -> crate::session::poller::SessionIdObservation {
        let mut observation =
            crate::session::poller::SessionIdObservation::instance_sidecar(sid, None);
        if let Some(active) = self.active_execution.as_ref().filter(|active| {
            matches!(
                active.capture,
                Some(CaptureContext::Prime {
                    runtime: Some(_),
                    ..
                })
            )
        }) {
            observation.execution = Some(active.clone());
            observation.scope_to(active.binding.clone());
        }
        observation
    }

    pub(super) fn prime_live_status(&self, shell_pid: u32) -> Option<Status> {
        if let Some(observation) = self
            .prime_runtime()
            .and_then(|runtime| runtime.observe(false))
        {
            return Some(observation.status());
        }
        let mut native_alive = false;
        for process in crate::process::foreground_arguments(shell_pid) {
            if process
                .argv
                .get(1)
                .is_some_and(|arg| arg == "__prime-agent")
            {
                native_alive = true;
                continue;
            }
            let name = process
                .argv
                .first()
                .and_then(|arg| Path::new(arg).file_name())
                .and_then(|name| name.to_str());
            if !matches!(name, Some("prime-agent" | ".prime-agent-wrapped")) {
                continue;
            }
            native_alive = true;
            if let Some(options) = parse_prime_agent_launch_options(&process.argv) {
                if !matches!(options.mode.as_deref(), Some("rpc" | "acp" | "daemon"))
                    && (options.is_print() || process.stdin_is_tty == Some(false))
                {
                    return Some(Status::Running);
                }
            }
        }
        native_alive.then_some(Status::Unknown)
    }

    fn prime_fork_parent_handoff<'a>(
        &'a self,
        execution: &super::execution::NativeExecution,
    ) -> anyhow::Result<Option<(&'a ConversationBinding, &'a std::path::Path)>> {
        let ResumeIntent::Fork { from } = &self.resume_intent else {
            return Ok(None);
        };
        let parent = self
            .resume_binding
            .as_ref()
            .filter(|binding| binding.session_id == *from)
            .context("Prime fork has no parent conversation binding")?;
        let parent_execution = parent
            .execution
            .as_ref()
            .filter(|bound| bound.agent == execution.binding.agent)
            .context("Prime fork needs a qualified parent conversation")?;
        if Self::execution_identity_matches(parent_execution, &execution.binding) {
            return Ok(None);
        }
        self.validate_conversation_target(
            parent_execution,
            execution.target_session_id.as_deref(),
        )?;
        anyhow::ensure!(
            parent_execution.filesystem == execution.binding.filesystem,
            "the parent's Prime store is not on this launch's filesystem"
        );
        let [parent_sessions] = parent_execution.stores.as_slice() else {
            anyhow::bail!("the parent records no single Prime sessions directory");
        };
        Ok(Some((parent, parent_sessions)))
    }

    pub(super) fn validate_prime_fork_handoff(
        &self,
        execution: &super::execution::NativeExecution,
    ) -> anyhow::Result<bool> {
        let Some(CaptureContext::Prime { plan, .. }) = &execution.capture else {
            return Ok(false);
        };
        let Some((parent, parent_sessions)) = self.prime_fork_parent_handoff(execution)? else {
            return Ok(false);
        };
        crate::session::capture::validate_prime_root_transcript(
            parent_sessions,
            &parent.session_id,
            &plan.container_cwd,
        )?;
        Ok(true)
    }

    /// Copy only after read-only preflight; publish the rebound target after launch.
    pub(super) fn hand_off_prime_fork_parent(
        &mut self,
        execution: &super::execution::NativeExecution,
    ) -> anyhow::Result<Option<ConversationState>> {
        let Some(CaptureContext::Prime { plan, .. }) = &execution.capture else {
            return Ok(None);
        };
        let Some((parent, parent_sessions)) = self.prime_fork_parent_handoff(execution)? else {
            return Ok(None);
        };
        crate::session::capture::hand_off_prime_root_transcript(
            parent_sessions,
            &plan.store,
            &plan.session_dir,
            &parent.session_id,
            &plan.container_cwd,
        )
        .context("cannot hand the parent's Prime conversation to the fork")?;
        let parent = parent.clone();
        let prior = self.conversation_state();
        let rebound = ConversationBinding {
            execution: Some(execution.binding.clone()),
            ..parent
        };
        self.retroactive_capture_excludes.insert(rebound.clone());
        self.resume_binding = Some(rebound);
        Ok(Some(prior))
    }

    pub(super) fn prime_runtime(&self) -> Option<&crate::process::prime::PrimeRuntime> {
        match self.active_execution.as_ref()?.capture.as_ref()? {
            CaptureContext::Prime { runtime, .. } => runtime.as_ref(),
            CaptureContext::PrimeStatus(runtime) => Some(runtime),
            _ => None,
        }
    }

    pub(super) fn prime_published_conversation(
        &self,
    ) -> Option<crate::session::poller::SessionIdObservation> {
        let PrimeRootPublication::Ready(sid) = self.prime_root_publication(false)? else {
            return None;
        };
        Some(self.prime_root_observation(sid))
    }

    fn attributable_prime_root_in(
        &self,
        publication: PrimeRootPublication,
    ) -> Option<Option<String>> {
        match publication {
            PrimeRootPublication::Ready(id)
                if !self.is_capture_excluded(
                    &id,
                    self.active_execution.as_ref().map(|active| &active.binding),
                ) =>
            {
                Some(Some(id))
            }
            PrimeRootPublication::Pending(id)
                if !self.is_capture_excluded(
                    &id,
                    self.active_execution.as_ref().map(|active| &active.binding),
                ) =>
            {
                Some(None)
            }
            _ => None,
        }
    }

    pub(super) fn absorb_published_prime_session(
        &mut self,
        publication: Option<PrimeRootPublication>,
    ) -> bool {
        if !self.resume_intent.is_default() {
            return false;
        }
        let Some(target) =
            publication.and_then(|publication| self.attributable_prime_root_in(publication))
        else {
            return false;
        };
        let binding = target.as_ref().and_then(|sid| {
            self.prime_root_observation(sid.clone())
                .conversation_binding()
        });
        if self.agent_session_id == target && self.agent_session_binding == binding {
            return false;
        }
        self.set_agent_conversation(target, binding, None);
        true
    }

    pub(crate) fn prime_root_poll_fn(
        &self,
        plan: PrimeAgentCapturePlan,
    ) -> Option<Box<dyn Fn() -> Option<PrimeRootPublication> + Send + 'static>> {
        if self
            .prime_agent_launch_options()
            .is_some_and(|options| options.is_print())
        {
            return None;
        }
        let runtime = self.prime_runtime().cloned();
        Some(Box::new(move || {
            validated_prime_root_publication(&plan, runtime.as_ref()?.observe(false)?.as_ref())
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::instance::test_helpers::*;

    #[test]
    #[serial_test::serial]

    fn prime_capture_plan_follows_upstream_session_directory_precedence() {
        let tmp = tempfile::tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(&tmp.path().join("app"));
        let project = tmp.path().join("project");
        std::fs::create_dir_all(project.join(".prime/agent")).unwrap();
        let mut inst = tool_instance("prime-agent", project.to_str().unwrap());
        inst.sandbox_info = Some(test_sandbox("prime-plan", Some("/workspace/project")));
        admit_sandbox_fixture(&inst);
        let store = inst.sandbox_capture_store_dir().unwrap();
        std::fs::create_dir_all(&store).unwrap();
        let mut config = inst.build_container_config_for_test().unwrap();

        let plan = inst
            .prime_agent_capture_plan_with(&config, store.clone())
            .unwrap();
        assert_eq!(plan.session_dir, Path::new("sessions"));

        inst.extra_args = "--mode daemon --mode invalid".to_string();
        assert!(inst
            .prime_agent_capture_plan_with(&config, store.clone())
            .is_none());
        inst.extra_args.clear();

        #[cfg(unix)]
        {
            let settings = store.join("settings.json");
            std::fs::write(
                store.join("linked-settings.json"),
                r#"{"sessionDir":"/root/.prime/agent/custom"}"#,
            )
            .unwrap();
            std::os::unix::fs::symlink("linked-settings.json", &settings).unwrap();
            assert!(
                inst.prime_agent_capture_plan_with(&config, store.clone())
                    .is_none(),
                "unknown settings must not select the default session directory"
            );
            inst.extra_args = "--session-dir /root/.prime/agent/explicit".to_string();
            let plan = inst
                .prime_agent_capture_plan_with(&config, store.clone())
                .unwrap();
            assert_eq!(plan.session_dir, Path::new("explicit"));
            inst.extra_args = "--session-dir /tmp/outside".to_string();
            assert!(inst
                .prime_agent_capture_plan_with(&config, store.clone())
                .is_none());
            inst.extra_args.clear();
            std::fs::remove_file(&settings).unwrap();
        }
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
    fn prepared_prime_launch_refreshes_resident_root_without_overwriting_peer() {
        let tmp = tempfile::tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(&tmp.path().join("app"));
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let mut inst = tool_instance("prime-agent", project.to_str().unwrap());
        inst.source_profile = "prime-resident-root-refresh".into();
        inst.extra_args = "--session-dir /root/.prime/agent/sessions".into();
        inst.sandbox_info = Some(test_sandbox("prime-resident", Some("/workspace/project")));
        admit_sandbox_fixture(&inst);
        let config = inst.build_container_config_for_test().unwrap();
        let _transport = install_container_transport(tmp.path(), "prime-resident", &config.volumes);
        let parent = "018f47a6-7b80-7cc3-98a2-37b5f486b2a1";
        let newer = "018f47a6-7b80-7cc3-98a2-37b5f486b2a3";
        inst.set_agent_conversation(Some(parent.into()), None, None);
        let storage =
            crate::session::storage::Storage::new_unwatched(&inst.source_profile).unwrap();
        storage
            .update(|rows, _| {
                rows.push(inst.clone());
                Ok(())
            })
            .unwrap();
        let prepared = inst
            .prepare_launch_command(inst.conversation_state())
            .unwrap();
        assert!(prepared.command.as_deref().unwrap().contains(parent));

        let publication = PrimeRootPublication::Ready(newer.into());
        let mut excluded = inst.clone();
        excluded
            .retroactive_capture_excludes
            .insert(ConversationBinding::unknown(newer));
        let prior = excluded.conversation_state();
        let prepared_excluded = excluded.prepare_launch_command(prior).unwrap();
        let excluded_prepared = excluded
            .refresh_prepared_prime_launch_after_pane_stop(
                prepared_excluded,
                Some(publication.clone()),
            )
            .unwrap();
        assert!(!excluded_prepared
            .command
            .as_deref()
            .unwrap()
            .contains(newer));

        let refreshed = inst
            .refresh_prepared_prime_launch_after_pane_stop(prepared, Some(publication))
            .unwrap();
        assert!(refreshed.command.as_deref().unwrap().contains(newer));
        assert_eq!(
            inst.persist_session_id(
                &inst.source_profile.clone(),
                &refreshed.expected_conversation
            ),
            SidPersistOutcome::Published
        );
        assert_eq!(
            storage.load().unwrap()[0].agent_session_id.as_deref(),
            Some(newer)
        );

        let peer = "018f47a6-7b80-7cc3-98a2-37b5f486b2a2";
        storage
            .update(|rows, _| {
                rows[0].agent_session_id = Some(peer.into());
                Ok(())
            })
            .unwrap();
        assert_eq!(
            inst.persist_session_id(
                &inst.source_profile.clone(),
                &refreshed.expected_conversation,
            ),
            SidPersistOutcome::Published
        );
        assert_eq!(
            storage.load().unwrap()[0].agent_session_id.as_deref(),
            Some(peer)
        );
        assert_eq!(inst.agent_session_id.as_deref(), Some(peer));
    }

    /// A fork child's private store holds nothing of the parent's, so its launch
    /// would be refused at the identity check. The handoff puts the parent's
    /// transcript in the child's store and rebinds the target there, and the
    /// captured child then resumes from its own store.
    #[test]
    #[serial_test::serial]
    fn a_sandboxed_prime_fork_child_launches_from_its_own_store_and_resumes_there() {
        let tmp = tempfile::tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(&tmp.path().join("app"));
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let (parent_id, child_id) = (
            "018f47a6-7b80-7cc3-98a2-37b5f486b2a1",
            "018f47a6-7b80-7cc3-98a2-37b5f486b2a9",
        );
        let sandboxed = |name: &str| {
            let mut inst = tool_instance("prime-agent", project.to_str().unwrap());
            inst.source_profile = "prime-fork-handoff".into();
            inst.extra_args = "--session-dir /root/.prime/agent/sessions".into();
            inst.sandbox_info = Some(test_sandbox(name, Some("/workspace/project")));
            admit_sandbox_fixture(&inst);
            inst
        };
        let plan_of = |inst: &Instance| {
            inst.prime_agent_capture_plan(inst.prime_agent_capture_options().unwrap())
                .unwrap()
        };
        let write_transcript = |plan: &PrimeAgentCapturePlan, id: &str| {
            let dir = plan.store.join(&plan.session_dir);
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join(format!("{id}.jsonl"));
            std::fs::write(
                &path,
                format!(
                    "{{\"type\":\"session\",\"id\":\"{id}\",\"cwd\":\"{}\",\"rlmDepth\":0}}\n{{\"type\":\"message\"}}\n",
                    plan.container_cwd
                ),
            )
            .unwrap();
            path
        };

        let parent = sandboxed("prime-fork-parent");
        let parent_plan = plan_of(&parent);
        let parent_transcript = write_transcript(&parent_plan, parent_id);
        let parent_bytes = std::fs::read(&parent_transcript).unwrap();
        let parent_binding = {
            let config = parent.build_container_config_for_test().unwrap();
            let _transport = install_container_transport(
                &tmp.path().join("parent-transport"),
                "prime-fork-parent",
                &config.volumes,
            );
            parent.resolve_native_execution(None).unwrap().binding
        };

        let mut child = sandboxed("prime-fork-child");
        let child_plan = plan_of(&child);
        assert_ne!(child_plan.store, parent_plan.store);
        child.resume_intent = ResumeIntent::Fork {
            from: parent_id.into(),
        };
        child.resume_binding = Some(ConversationBinding {
            session_id: parent_id.into(),
            execution: Some(parent_binding.clone()),
            provenance: ConversationProvenance::Observed,
            transcript_path: None,
        });
        child.agent_session_id = Some(child_id.into());
        let config = child.build_container_config_for_test().unwrap();
        let _transport = install_container_transport(
            &tmp.path().join("child-transport"),
            "prime-fork-child",
            &config.volumes,
        );

        let target = child_plan
            .store
            .join(&child_plan.session_dir)
            .join(format!("{parent_id}.jsonl"));
        let before = child.conversation_state();
        let invalid_store = tmp.path().join("invalid-parent-store");
        std::fs::create_dir(&invalid_store).unwrap();
        std::fs::write(
            invalid_store.join(format!("{parent_id}.jsonl")),
            format!("{{\"type\":\"session\",\"id\":\"{parent_id}\",\"cwd\":\"/workspace/foreign\",\"rlmDepth\":0}}\n"),
        ).unwrap();
        for reason in ["agent", "filesystem", "id", "missing", "cwd"] {
            let mut invalid = child.clone();
            let binding = invalid.resume_binding.as_mut().unwrap();
            match reason {
                "agent" => binding.execution.as_mut().unwrap().agent = "omp".into(),
                "filesystem" => binding.execution.as_mut().unwrap().filesystem = "foreign".into(),
                "id" => binding.session_id = child_id.into(),
                "missing" => {
                    binding.execution.as_mut().unwrap().stores = vec![tmp.path().join("missing")]
                }
                "cwd" => binding.execution.as_mut().unwrap().stores = vec![invalid_store.clone()],
                _ => unreachable!(),
            }
            let original = invalid.conversation_state();
            assert!(invalid.preflight_resume_launch().is_err(), "{reason}");
            assert_eq!(invalid.conversation_state(), original);
            assert!(
                !target.exists(),
                "preflight must not copy a parent transcript"
            );
        }
        child.preflight_resume_launch().unwrap();
        assert_eq!(child.conversation_state(), before);
        assert!(!target.exists(), "valid preflight is read-only");
        assert_eq!(std::fs::read(&parent_transcript).unwrap(), parent_bytes);

        let prepared = child
            .prepare_launch_command(child.conversation_state())
            .unwrap();
        let command = prepared.command.as_deref().unwrap();
        assert!(
            command.contains(&format!("--fork {parent_id}")),
            "{command}"
        );
        assert!(!command.contains("--resume"), "{command}");
        assert!(prepared.fork_mints_child);
        assert_eq!(
            std::fs::read(
                child_plan
                    .store
                    .join(&child_plan.session_dir)
                    .join(format!("{parent_id}.jsonl"))
            )
            .unwrap(),
            parent_bytes
        );
        assert_eq!(std::fs::read(&parent_transcript).unwrap(), parent_bytes);
        let rebound = prepared
            .canonical_conversation
            .as_ref()
            .and_then(|state| state.resume_binding.as_ref())
            .and_then(|binding| binding.execution.as_ref())
            .expect("the fork target is rebound to the child's context");
        assert_ne!(rebound.stores, parent_binding.stores);
        assert_eq!(
            child
                .resume_binding
                .as_ref()
                .and_then(|b| b.execution.as_ref()),
            Some(&parent_binding),
            "a prepared launch leaves the row untouched until it is adopted"
        );

        write_transcript(&child_plan, child_id);
        child.adopt_conversation_state(prepared.canonical_conversation.unwrap());
        child.resume_intent = ResumeIntent::Default;
        child.resume_binding = None;
        child.set_agent_conversation(Some(child_id.into()), None, None);
        let restarted = child
            .prepare_launch_command(child.conversation_state())
            .unwrap();
        let command = restarted.command.as_deref().unwrap();
        assert!(
            command.contains(&format!("--resume {child_id}")),
            "{command}"
        );
        assert!(!command.contains(parent_id), "{command}");
    }

    #[test]
    fn native_root_publication_requires_header_corroboration_and_preserves_empty_boundary() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("sessions")).unwrap();
        let plan = PrimeAgentCapturePlan {
            store: temp.path().into(),
            session_dir: "sessions".into(),
            container_session_dir: "/root/.prime/agent/sessions".into(),
            container_cwd: "/workspace".into(),
        };
        let id = "018f47a6-7b80-7cc3-98a2-37b5f486b2a1";
        let observation = |file: serde_json::Value| {
            serde_json::from_value::<crate::process::prime::PrimeObservation>(serde_json::json!({"sessionId": id, "sessionFile":file,"cwd":"/workspace", "activeSessionId":"daemon-address", "rlmDepth":0,"attachedClients":1,"workerState":"ready","lifecycle":"active","activity":"idle","isStreaming":false,"isCompacting":false,"hasRunningSubagents":false,"isBashRunning":false,"isSessionActive":false})).unwrap()
        };
        assert_eq!(
            validated_prime_root_publication(&plan, &observation(serde_json::Value::Null)),
            Some(PrimeRootPublication::Pending(id.into()))
        );
        let file = serde_json::json!("/root/.prime/agent/sessions/root.jsonl");
        for (header_id, depth, cwd, expected) in [
            (id, 0, "/workspace", true),
            ("other-root", 0, "/workspace", false),
            (id, 1, "/workspace", false),
            (id, 0, "/other", false),
        ] {
            std::fs::write(
                temp.path().join("sessions/root.jsonl"),
                format!(
                    "{}\n",
                    serde_json::json!({"type":"session","id":header_id,"rlmDepth":depth,"cwd":cwd})
                ),
            )
            .unwrap();
            assert_eq!(
                validated_prime_root_publication(&plan, &observation(file.clone())),
                expected.then(|| PrimeRootPublication::Ready(id.into()))
            );
        }
        assert_eq!(
            validated_prime_root_publication(
                &plan,
                &observation(serde_json::json!("/outside/root.jsonl"))
            ),
            None
        );
        #[cfg(unix)]
        {
            std::fs::remove_file(temp.path().join("sessions/root.jsonl")).unwrap();
            std::os::unix::fs::symlink(
                "/outside/root.jsonl",
                temp.path().join("sessions/root.jsonl"),
            )
            .unwrap();
            assert_eq!(
                validated_prime_root_publication(&plan, &observation(file)),
                None
            );
        }
    }

    #[test]
    fn native_settings_archive_rejects_links_oversize_and_multiple_files() {
        let settings = br#"{"sessionDir":"/root/.prime/agent/history"}"#;
        let large = vec![b' '; 64 * 1024 + 1];
        for (kind, oversized, extra, accepted) in [
            (tar::EntryType::Regular, false, false, true),
            (tar::EntryType::Symlink, false, false, false),
            (tar::EntryType::Regular, true, false, false),
            (tar::EntryType::Regular, false, true, false),
        ] {
            let mut builder = tar::Builder::new(Vec::new());
            let mut header = tar::Header::new_gnu();
            header.set_path("settings.json").unwrap();
            header.set_entry_type(kind);

            let data: &[u8] = if kind.is_symlink() {
                b""
            } else if oversized {
                &large
            } else {
                settings
            };
            if kind.is_symlink() {
                header.set_link_name("auth.json").unwrap();
            }
            header.set_size(data.len() as u64);
            header.set_cksum();
            builder.append(&header, data).unwrap();
            if extra {
                builder.append(&header, data).unwrap();
            }
            let bytes = builder.into_inner().unwrap();
            let result = settings_from_native_archive(&bytes);
            if accepted {
                assert_eq!(
                    result.unwrap().unwrap()["sessionDir"],
                    "/root/.prime/agent/history"
                );
            } else {
                assert!(result.is_err());
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn native_prime_resolution_failure_never_downgrades_to_an_unowned_launch() {
        use crate::session::{ManagedResumeRefused, ResumeIntent};

        let _app = crate::session::test_support::isolate_app_dir();
        let sid = "018f47a6-7b80-7cc3-98a2-37b5f486b2a1";
        for (intent, recorded_sid, extra_args, refused) in [
            (ResumeIntent::Default, Some(sid), "", true),
            (ResumeIntent::Default, None, "", false),
            (ResumeIntent::Cleared, Some(sid), "", false),
            (ResumeIntent::Use(sid.into()), Some(sid), "", false),
            (
                ResumeIntent::Fork { from: sid.into() },
                Some(sid),
                "",
                false,
            ),
            (ResumeIntent::Default, Some(sid), "--no-session", true),
        ] {
            let mut instance = tool_instance("prime-agent", "/tmp");
            instance.command = "prime-agent".into();
            instance.agent_session_id = recorded_sid.map(str::to_owned);
            instance.resume_intent = intent;
            instance.extra_args = extra_args.into();
            let agent = instance.default_selector_agent().unwrap();
            assert_eq!(agent.name, "prime-agent");
            assert!(instance.launch_invokes_resolved_agent_directly(agent));
            assert_eq!(
                instance.prime_agent_launch_options().is_some(),
                extra_args.is_empty()
            );
            assert_eq!(
                instance.conversation_target().is_some(),
                !matches!(instance.resume_intent, ResumeIntent::Cleared) && recorded_sid.is_some()
            );
            let expected = instance.conversation_state();
            let before = serde_json::to_value(&instance).unwrap();
            super::super::execution::FAIL_NEXT_NATIVE_RESOLUTION.with(|fail| fail.set(true));
            let result = instance.prepare_launch_command(expected.clone());
            super::super::execution::FAIL_NEXT_NATIVE_RESOLUTION.with(|fail| fail.set(false));
            let error = result
                .err()
                .expect("Prime resolution failure must not produce an unowned native command");
            assert_eq!(instance.conversation_state(), expected);
            assert_eq!(serde_json::to_value(&instance).unwrap(), before);
            assert_eq!(
                error.root_cause().to_string(),
                "injected transient native resolution failure"
            );
            assert_eq!(
                error.is::<ManagedResumeRefused>(),
                refused,
                "intent={:?}, recorded_sid={recorded_sid:?}, args={extra_args:?}: {error:#}",
                instance.resume_intent
            );
        }
    }
}
