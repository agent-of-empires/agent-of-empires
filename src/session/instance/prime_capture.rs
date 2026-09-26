//! Prime Agent capture: launch options, session directory plan, root publication.

use super::identity_sidecar::{read_sandbox_sidecar_file, SESSION_SIDECAR_MAX_BYTES};
use super::*;
use crate::agents::SessionCaptureBackend;
use crate::session::config::container_config::PRIME_AGENT_DIR_IN_CONTAINER;

const PRIME_AGENT_HEADER_MAX_BYTES: u64 = 64 * 1024;

#[derive(Default)]
pub(super) struct PrimeAgentLaunchOptions {
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
            "--mode" | "--cwd" | "--session-dir" => {
                let value = words.get(index + 1)?.clone();
                match argument {
                    "--cwd" => options.cwd = Some(value),
                    "--session-dir" => options.session_dir = Some(value),
                    _ if matches!(value.as_str(), "text" | "json" | "rpc" | "acp" | "daemon") => {
                        options.mode = Some(value)
                    }
                    _ => {}
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
    instance_id: &str,
) -> Option<PrimeRootPublication> {
    use std::io::{BufRead as _, Read as _};

    #[derive(Deserialize)]
    struct Publication {
        id: String,
        path: PathBuf,
        cwd: String,
        #[serde(rename = "rlmDepth")]
        depth: u64,
    }
    let bytes = read_sandbox_sidecar_file(
        &plan.store,
        instance_id,
        "root_session",
        3 * SESSION_SIDECAR_MAX_BYTES,
    )?;
    let publication: Publication = serde_json::from_slice(&bytes).ok()?;
    let id = validated_session_id(publication.id)?;
    let path = &publication.path;
    let expected_cwd = crate::session::capture::canonicalize_or_raw(&plan.container_cwd);
    if publication.depth != 0
        || crate::session::capture::canonicalize_or_raw(&publication.cwd) != expected_cwd
        || !path.is_absolute()
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

    fn prime_agent_launch_options(&self) -> Option<PrimeAgentLaunchOptions> {
        let parsed = parse_launch_command(self.get_tool_command())?;
        let mut words = parsed.words;
        words.extend(shell_words::split(&self.extra_args).ok()?);
        let options = parse_prime_agent_launch_options(&words)?;
        (!options.no_session && options.mode.as_deref() != Some("daemon")).then_some(options)
    }

    pub(super) fn prime_agent_capture_plan(
        &self,
        options: PrimeAgentLaunchOptions,
    ) -> anyhow::Result<PrimeAgentCapturePlan> {
        let store = self
            .sandbox_capture_store_dir()
            .context("managed Prime store is unavailable")?;
        let config = self
            .build_container_config()
            .context("cannot build Prime container configuration")?;
        self.resolve_prime_agent_capture_plan(&config, store, options)
    }

    pub(super) fn prime_agent_capture_plan_with(
        &self,
        config: &crate::containers::ContainerConfig,
        store: PathBuf,
    ) -> Option<PrimeAgentCapturePlan> {
        let options = self.prime_agent_capture_options()?;
        self.resolve_prime_agent_capture_plan(config, store, options)
            .inspect_err(|error| {
                tracing::debug!(target: "session.capture", session = %self.id, reason = %format_args!("{error:#}"),
                    "Prime identity extension unavailable");
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
        )
    }

    fn resolve_prime_agent_layout(
        store: PathBuf,
        options: PrimeAgentLaunchOptions,
        launch_cwd: &Path,
        (home, agent_dir): (&Path, &Path),
        environment_session_dir: Option<&str>,
        host_path_for: impl Fn(&Path, bool) -> Option<PathBuf>,
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
            let global_host_path = host_path_for(&global_container_path, false)
                .context("global Prime settings are not mapped to a readable host path")?;
            let project_host_path = host_path_for(&project_container_path, false)
                .context("project Prime settings are not mapped to a readable host path")?;
            let global =
                super::session_id::read_session_settings(&global_host_path).with_context(|| {
                    format!(
                        "cannot safely read Prime settings {}",
                        global_host_path.display()
                    )
                })?;
            let project = super::session_id::read_session_settings(&project_host_path)
                .with_context(|| {
                    format!(
                        "cannot safely read Prime settings {}",
                        project_host_path.display()
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

    pub(super) fn prime_root_publication(&self) -> Option<PrimeRootPublication> {
        if let Some(active) = &self.active_execution {
            let Some(CaptureContext::Prime {
                plan,
                sidecar: Some(_),
            }) = &active.capture
            else {
                return None;
            };
            return validated_prime_root_publication(plan, &self.id);
        }
        let plan = self.prime_agent_capture_plan(self.prime_agent_capture_options()?)
            .inspect_err(|error| {
                tracing::debug!(target: "session.capture", session = %self.id, reason = %format_args!("{error:#}"),
                    "Prime root publication cannot be attributed");
            })
            .ok()?;
        validated_prime_root_publication(&plan, &self.id)
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
                    sidecar: Some(_),
                    ..
                })
            )
        }) {
            observation.execution = Some(active.clone());
            observation.source = Some(active.binding.clone());
        }
        observation
    }

    pub(super) fn prime_published_conversation(
        &self,
    ) -> Option<crate::session::poller::SessionIdObservation> {
        let PrimeRootPublication::Ready(sid) = self.prime_root_publication()? else {
            return None;
        };
        Some(self.prime_root_observation(sid))
    }

    pub(super) fn attributable_prime_root(&self) -> Option<Option<String>> {
        match self.prime_root_publication()? {
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

    pub(super) fn absorb_published_prime_session(&mut self) -> bool {
        if !self.resume_intent.is_default() {
            return false;
        }
        let Some(target) = self.attributable_prime_root() else {
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

    pub(crate) fn prime_root_sidecar_poll_fn(
        &self,
        plan: PrimeAgentCapturePlan,
    ) -> Box<dyn Fn() -> Option<PrimeRootPublication> + Send + 'static> {
        let instance_id = self.id.clone();
        Box::new(move || validated_prime_root_publication(&plan, &instance_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::instance::test_helpers::*;

    #[test]
    #[serial_test::serial]
    fn sandboxed_prime_agent_uses_only_its_managed_store_poller() {
        let tmp = tempfile::TempDir::new().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(&tmp.path().join("app"));
        let mut inst = tool_instance("prime-agent", "/tmp/test");
        inst.sandbox_info = Some(test_sandbox("test", Some("/workspace/test")));
        admit_sandbox_fixture(&inst);

        assert_eq!(inst.try_retroactive_capture(), None);
        std::fs::create_dir_all(inst.sandbox_capture_store_dir().unwrap()).unwrap();
        inst.capture_started_at = Some(std::time::SystemTime::now());
        inst.maybe_start_poller_since(None);
        assert!(inst.session_id_poller.is_some());
        inst.stop_poller();
    }

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
    fn prime_unmaterialized_root_never_resumes_previous_history() {
        let tmp = tempfile::tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(&tmp.path().join("app"));
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let mut inst = tool_instance("prime-agent", project.to_str().unwrap());
        inst.sandbox_info = Some(test_sandbox("prime-empty", Some("/workspace/project")));
        admit_sandbox_fixture(&inst);
        let plan = inst
            .prime_agent_capture_plan(inst.prime_agent_capture_options().unwrap())
            .unwrap();
        let sessions = plan.store.join(&plan.session_dir);
        std::fs::create_dir_all(&sessions).unwrap();
        let old = "018f47a6-7b80-7cc3-98a2-37b5f486b2a1";
        let new = "018f47a6-7b80-7cc3-98a2-37b5f486b2a2";
        let header = |id: &str| {
            format!(
                "{}\n",
                serde_json::json!({
                    "type": "session", "id": id, "cwd": "/workspace/project", "rlmDepth": 0
                })
            )
        };
        std::fs::write(sessions.join("old.jsonl"), header(old)).unwrap();
        let record = plan
            .store
            .join("aoe-session")
            .join(&inst.id)
            .join("root_session");
        std::fs::create_dir_all(record.parent().unwrap()).unwrap();
        std::fs::write(
            &record,
            serde_json::json!({
                "id": new,
                "path": plan.container_session_dir.join("new.jsonl"),
                "cwd": "/workspace/project",
                "rlmDepth": 0
            })
            .to_string(),
        )
        .unwrap();
        inst.agent_session_id = Some(old.to_string());

        assert_eq!(
            inst.prime_root_publication(),
            Some(PrimeRootPublication::Pending(new.to_string()))
        );
        let persisted = serde_json::to_string(&inst).unwrap();
        let mut command = "prime-agent".to_string();
        let agent = inst.resolved_agent();
        assert!(!inst
            .apply_session_flags(&mut command, "test", agent, None)
            .unwrap());
        assert_eq!(command, "prime-agent");
        assert_eq!(inst.agent_session_id, None);

        let mut restarted: Instance = serde_json::from_str(&persisted).unwrap();
        let mut command = "prime-agent".to_string();
        assert!(!restarted
            .apply_session_flags(&mut command, "test", agent, None)
            .unwrap());
        assert_eq!(command, "prime-agent");

        // Materialization makes the new root resumable, never the unrelated old transcript.
        std::fs::write(sessions.join("new.jsonl"), header(new)).unwrap();
        let mut restarted: Instance = serde_json::from_str(&persisted).unwrap();
        let mut command = "prime-agent".to_string();
        assert!(restarted
            .apply_session_flags(&mut command, "test", agent, None)
            .unwrap());
        assert_eq!(command, format!("prime-agent --resume {new}"));
    }

    #[test]
    #[serial_test::serial]
    fn prime_root_publisher_rejects_child_and_resumes_parent() {
        if which::which("node").is_err() {
            eprintln!("skipping: node not found");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(&tmp.path().join("app"));
        let store = tmp.path().join("store");
        std::fs::create_dir_all(store.join("custom-sessions")).unwrap();
        let store = std::fs::canonicalize(store).unwrap();
        let sessions = store.join("custom-sessions");
        let mut inst = tool_instance("prime-agent", tmp.path().to_str().unwrap());
        inst.extra_args = "--session-dir /root/.prime/agent/custom-sessions".into();
        inst.sandbox_info = Some(test_sandbox("prime-root", Some("/workspace/project")));
        let parent = "018f47a6-7b80-7cc3-98a2-37b5f486b2a1";
        let child = "018f47a6-7b80-7cc3-98a2-37b5f486b2a2";
        for (name, id, depth) in [("parent", parent, 0), ("child", child, 1)] {
            std::fs::write(sessions.join(format!("{name}.jsonl")), format!("{}\n",
                serde_json::json!({"type":"session", "id":id, "rlmDepth":depth, "cwd":"/workspace/project"}))).unwrap();
        }
        let sidecar = store.join("aoe-session").join(&inst.id).join("session_id");
        let normal = store.join("pi-default/session_id");
        let extension = store.join("extensions/aoe-session-id.js");
        std::fs::create_dir_all(extension.parent().unwrap()).unwrap();
        std::fs::copy(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/assets/session/aoe-session-id.js"
            ),
            &extension,
        )
        .unwrap();
        let script = r#"
    import { readFileSync } from 'node:fs';
    import { pathToFileURL } from 'node:url';
    const extension = (await import(pathToFileURL(process.argv[1]).href)).default;
    process.chdir(process.argv[4]);
    async function emit(target, rootOnly, id, name, depth) {
      process.env.AOE_SESSION_ID_FILE = target;
      process.env.AOE_SESSION_ROOT_ONLY = rootOnly ? '1' : '0';
      let onStart;
      extension({ on(event, handler) { if (event === 'session_start') onStart = handler; } });
      await onStart({}, { sessionManager: {
    getSessionId: () => id,
    getSessionFile: () => 'custom-sessions/' + name + '.jsonl',
    getHeader: () => ({ rlmDepth: depth, cwd: '/workspace/project' }),
      } });
    }
    const [root, normal] = [process.argv[2], process.argv[3]];
    await emit(root, true, '018f47a6-7b80-7cc3-98a2-37b5f486b2a1', 'parent', 0);
    await emit(root, true, '018f47a6-7b80-7cc3-98a2-37b5f486b2a2', 'child', undefined);
    await emit(root, true, '018f47a6-7b80-7cc3-98a2-37b5f486b2a2', 'child', 1);
    await emit(normal, false, '018f47a6-7b80-7cc3-98a2-37b5f486b2a2', 'child', 1);
    process.stdout.write(readFileSync(normal, 'utf8'));
    "#;
        let output = std::process::Command::new("node")
            .args(["--input-type=module", "--eval", script])
            .arg(&extension)
            .arg(&sidecar)
            .arg(&normal)
            .arg(&store)
            // A host session source redirects the normal publication to a suffixed file.
            .env_remove(crate::hooks::SESSION_SOURCE_ENV)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), child);
        assert_eq!(
            std::fs::read_to_string(normal.parent().unwrap().join("session_path"))
                .unwrap()
                .trim(),
            "custom-sessions/child.jsonl"
        );
        let plan = PrimeAgentCapturePlan {
            store: store.clone(),
            session_dir: "custom-sessions".into(),
            container_session_dir: sessions.clone(),
            container_cwd: "/workspace/project".into(),
        };
        assert_eq!(
            validated_prime_root_publication(&plan, &inst.id),
            Some(PrimeRootPublication::Ready(parent.into()))
        );
        let record = sidecar.parent().unwrap().join("root_session");
        std::fs::write(&record, serde_json::json!({
            "id": child, "path": sessions.join("child.jsonl"), "cwd": "/workspace/project", "rlmDepth": 0
        }).to_string()).unwrap();
        assert_eq!(validated_prime_root_publication(&plan, &inst.id), None);
        std::fs::write(&record, serde_json::json!({
            "id": parent, "path": sessions.join("parent.jsonl"), "cwd": "/workspace/project", "rlmDepth": 0
        }).to_string()).unwrap();
        let binding = crate::session::ExecutionBinding {
            agent: "prime-agent".into(),
            stores: vec![store.clone()],
            configuration: Vec::new(),
            exported_default_store: false,
            cwd: "/workspace/project".into(),
            cwd_filesystem: "host".into(),
            filesystem: "host".into(),
        };
        inst.active_execution = Some(ActiveExecution {
            launch_id: "root-launch".into(),
            binding,
            capture: Some(CaptureContext::Prime {
                plan,
                sidecar: Some(SessionSidecarSource::SandboxDir(store)),
            }),
            container: None,
        });
        let mut command = "prime-agent".to_string();
        let agent = inst.resolved_agent();
        assert!(inst
            .apply_session_flags(&mut command, "test", agent, None)
            .unwrap());
        assert_eq!(command, format!("prime-agent --resume {parent}"));
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
        inst.sandbox_info = Some(test_sandbox("prime-resident", Some("/workspace/project")));
        admit_sandbox_fixture(&inst);
        let plan = inst
            .prime_agent_capture_plan(inst.prime_agent_capture_options().unwrap())
            .unwrap();
        let sessions = plan.store.join(&plan.session_dir);
        std::fs::create_dir_all(&sessions).unwrap();
        let record = plan
            .store
            .join("aoe-session")
            .join(&inst.id)
            .join("root_session");
        std::fs::create_dir_all(record.parent().unwrap()).unwrap();
        let parent = "018f47a6-7b80-7cc3-98a2-37b5f486b2a1";
        let newer = "018f47a6-7b80-7cc3-98a2-37b5f486b2a3";
        let publish = |id: &str, name: &str| {
            std::fs::write(sessions.join(format!("{name}.jsonl")), format!("{}\n",
                serde_json::json!({"type": "session", "id": id, "rlmDepth": 0, "cwd": plan.container_cwd}))).unwrap();
            std::fs::write(
                &record,
                serde_json::json!({
                    "id": id, "path": plan.container_session_dir.join(format!("{name}.jsonl")),
                    "cwd": plan.container_cwd, "rlmDepth": 0
                })
                .to_string(),
            )
            .unwrap();
        };
        publish(parent, "parent");
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

        publish(newer, "newer");
        let mut excluded = inst.clone();
        excluded
            .retroactive_capture_excludes
            .insert(ConversationBinding::unknown(newer));
        let prior = excluded.conversation_state();
        let prepared_excluded = excluded.prepare_launch_command(prior).unwrap();
        let excluded_prepared = excluded
            .refresh_prepared_prime_launch_after_pane_stop(prepared_excluded)
            .unwrap();
        assert!(!excluded_prepared
            .command
            .as_deref()
            .unwrap()
            .contains(newer));

        let refreshed = inst
            .refresh_prepared_prime_launch_after_pane_stop(prepared)
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
}
