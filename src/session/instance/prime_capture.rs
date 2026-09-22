//! Prime Agent capture: launch options, session directory plan, root publication.

use super::identity_sidecar::{read_sandbox_sidecar_file, SESSION_SIDECAR_MAX_BYTES};
use super::*;
use crate::agents::SessionCaptureBackend;
use crate::session::config::container_config::PRIME_AGENT_DIR_IN_CONTAINER;

const PRIME_AGENT_HEADER_MAX_BYTES: u64 = 64 * 1024;
const PRIME_AGENT_SETTINGS_MAX_BYTES: usize = 64 * 1024;

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

fn resolve_prime_agent_path(value: &str, cwd: &Path) -> PathBuf {
    let expanded = match value.strip_prefix('~') {
        Some("") => PathBuf::from("/root"),
        Some(rest) if rest.starts_with('/') => Path::new("/root").join(&rest[1..]),
        _ => PathBuf::from(value),
    };
    crate::git::template::lexical_normalize(&cwd.join(expanded))
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
        anyhow::ensure!(
            config.uses_default_container_home(),
            "Prime capture requires the default container HOME"
        );

        let launch_cwd = PathBuf::from(self.container_workdir());
        let container_cwd = options.cwd.as_deref().map_or_else(
            || launch_cwd.clone(),
            |cwd| resolve_prime_agent_path(cwd, &launch_cwd),
        );
        anyhow::ensure!(
            container_cwd.is_absolute(),
            "Prime working directory is not absolute"
        );

        let environment_value = |key: &str| {
            config
                .environment
                .iter()
                .find(|entry| entry.key() == key)
                .map(|entry| entry.value())
        };
        let configured_session_dir = options
            .session_dir
            .filter(|value| !value.is_empty())
            .or_else(|| {
                environment_value("PRIME_AGENT_SESSION_DIR")
                    .or_else(|| environment_value("PRIME_AGENT_CODING_AGENT_SESSION_DIR"))
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
            });

        let session_dir_value = match configured_session_dir {
            Some(value) => value,
            None => {
                let read_settings = |container_path: PathBuf, scope: &str| {
                    let host_path = config
                        .host_path_for_container_path(&container_path, false)
                        .with_context(|| {
                            format!("{scope} Prime settings are not mapped to a readable host path")
                        })?;
                    read_prime_agent_settings(&host_path).with_context(|| {
                        format!("cannot safely read Prime settings {}", host_path.display())
                    })
                };
                let global = read_settings(
                    Path::new(PRIME_AGENT_DIR_IN_CONTAINER).join("settings.json"),
                    "global",
                )?;
                let project =
                    read_settings(container_cwd.join(".prime/agent/settings.json"), "project")?;
                let setting = |settings: &Option<serde_json::Map<String, serde_json::Value>>| {
                    settings.as_ref().and_then(|s| s.get("sessionDir")).cloned()
                };
                match setting(&project).or_else(|| setting(&global)) {
                    Some(serde_json::Value::String(value)) => value,
                    Some(serde_json::Value::Null) | None => {
                        format!("{PRIME_AGENT_DIR_IN_CONTAINER}/sessions")
                    }
                    Some(_) => {
                        anyhow::bail!("Prime sessionDir setting is neither a string nor null")
                    }
                }
            }
        };

        let container_session_dir = resolve_prime_agent_path(&session_dir_value, &container_cwd);
        let session_dir = container_session_dir
            .strip_prefix(Path::new(PRIME_AGENT_DIR_IN_CONTAINER))
            .context("Prime session directory is outside the managed store")?
            .to_path_buf();
        let mapped = config
            .host_path_for_container_path(&container_session_dir, true)
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
        let plan = self.prime_agent_capture_plan(self.prime_agent_capture_options()?)
            .inspect_err(|error| {
                tracing::debug!(target: "session.capture", session = %self.id, reason = %format_args!("{error:#}"),
                    "Prime root publication cannot be attributed");
            })
            .ok()?;
        validated_prime_root_publication(&plan, &self.id)
    }

    /// The published root, unless excluded: `Some(Some(id))` when ready,
    /// `Some(None)` when its transcript is still empty.
    pub(super) fn attributable_prime_root(&self) -> Option<Option<String>> {
        match self.prime_root_publication()? {
            PrimeRootPublication::Ready(id) if !self.retroactive_capture_excludes.contains(&id) => {
                Some(Some(id))
            }
            PrimeRootPublication::Pending(id)
                if !self.retroactive_capture_excludes.contains(&id) =>
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
        if self.agent_session_id == target {
            return false;
        }
        self.agent_session_id = target;
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
        if which::which("node").is_err() {
            eprintln!("skipping: node not found on PATH");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(&tmp.path().join("app"));
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let mut inst = tool_instance("prime-agent", project.to_str().unwrap());
        inst.sandbox_info = Some(test_sandbox("prime-empty", Some("/workspace/project")));
        inst.build_launch_command().unwrap();
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
        inst.agent_session_id = Some(old.to_string());
        let prepared = inst.prepare_launch_command().unwrap();
        let persisted = serde_json::to_string(&inst).unwrap();
        let sidecar = plan
            .store
            .join("aoe-session")
            .join(&inst.id)
            .join("session_id");
        let script = r#"
import { pathToFileURL } from "node:url";
const extension = (await import(pathToFileURL(process.argv[1]).href)).default;
let publish;
extension({ on(event, callback) { if (event === "session_start") publish = callback; } });
await publish({}, { sessionManager: {
  getSessionId: () => process.argv[2],
  getSessionFile: () => process.argv[3],
  getHeader: () => ({ rlmDepth: 0, cwd: "/workspace/project" }),
} });
"#;
        let output = std::process::Command::new("node")
            .args(["--input-type=module", "--eval", script])
            .arg(plan.store.join("extensions/aoe-session-id.js"))
            .arg(new)
            .arg(plan.container_session_dir.join("new.jsonl"))
            .env("AOE_SESSION_ID_FILE", &sidecar)
            .env("AOE_SESSION_ROOT_ONLY", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let poll = crate::session::capture::prime_agent_poll_fn_sandboxed(
            inst.prime_root_sidecar_poll_fn(plan.clone()),
            plan.store.clone(),
            plan.session_dir.clone(),
            plan.container_cwd.clone(),
            inst.id.clone(),
            0.0,
            HashSet::new(),
        );
        assert_eq!(
            poll(),
            None,
            "an empty new root must suppress fallback to old history"
        );
        for (intent, expected) in [
            (
                ResumeIntent::Use(old.to_string()),
                (Some(old.to_string()), true),
            ),
            (ResumeIntent::Cleared, (None, false)),
            (
                ResumeIntent::Fork {
                    from: old.to_string(),
                },
                (Some(old.to_string()), false),
            ),
        ] {
            let mut explicit = inst.clone();
            explicit.resume_intent = intent;
            assert_eq!(explicit.acquire_session_id_with(&|_| None), expected);
        }
        let refreshed = inst
            .refresh_prepared_prime_launch_after_pane_stop(prepared)
            .unwrap();
        assert!(!refreshed.command.as_deref().unwrap().contains("--resume"));
        for _ in 0..2 {
            inst.clear_pane_identity_sidecar();
            let mut restarted: Instance = serde_json::from_str(&persisted).unwrap();
            let mut command = "prime-agent".to_string();
            assert!(!restarted.apply_session_flags(&mut command, "test").unwrap());
            assert_eq!(command, "prime-agent");
            assert_eq!(restarted.agent_session_id, None);
        }
        std::fs::create_dir(sessions.join("new.jsonl")).unwrap();
        assert_eq!(
            poll().as_deref(),
            Some(old),
            "a non-regular leaf is not an empty root"
        );
        let mut uncertain: Instance = serde_json::from_str(&persisted).unwrap();
        assert_eq!(
            uncertain.acquire_session_id_with(&|_| None),
            (Some(old.to_string()), true)
        );
        std::fs::remove_dir(sessions.join("new.jsonl")).unwrap();
        let unavailable = sessions.with_extension("unavailable");
        std::fs::rename(&sessions, &unavailable).unwrap();
        let mut uncertain: Instance = serde_json::from_str(&persisted).unwrap();
        assert_eq!(
            uncertain.acquire_session_id_with(&|_| None),
            (Some(old.to_string()), true)
        );
        std::fs::rename(&unavailable, &sessions).unwrap();
        std::fs::write(sessions.join("new.jsonl"), header(new)).unwrap();
        assert_eq!(poll().as_deref(), Some(new));
        let mut restarted: Instance = serde_json::from_str(&persisted).unwrap();
        let mut command = "prime-agent".to_string();
        assert!(restarted.apply_session_flags(&mut command, "test").unwrap());
        assert_eq!(command, format!("prime-agent --resume {new}"));
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
        let mut inst = tool_instance("prime-agent", project.to_str().unwrap());
        inst.extra_args = "--session-dir /root/.prime/agent/custom-sessions".to_string();
        inst.sandbox_info = Some(test_sandbox("prime-root-only", Some("/workspace/project")));

        inst.build_launch_command().unwrap();
        let store = inst.sandbox_capture_store_dir().unwrap();
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
        let root_sidecar = sidecar.parent().unwrap().join("root_session");
        let publish_root = |id: &str, file: &str| {
            std::fs::write(
                &root_sidecar,
                serde_json::json!({
                    "id": id, "path": format!("/root/.prime/agent/custom-sessions/{file}"),
                    "cwd": "/workspace/project", "rlmDepth": 0,
                })
                .to_string(),
            )
            .unwrap();
        };
        let default_sidecar = tmp.path().join("pi-default/session_id");
        let default_path_sidecar = default_sidecar.parent().unwrap().join("session_path");
        let script = r#"
import { readFileSync } from "node:fs";
import { pathToFileURL } from "node:url";
import { dirname, join } from "node:path";

const extension = (await import(pathToFileURL(process.argv[1]).href)).default;
process.chdir(process.argv[4]);
const context = (id, path, rlmDepth) => ({
  sessionManager: {
    getSessionId: () => id,
    getSessionFile: () => path,
    getHeader: () => ({ rlmDepth, cwd: "/workspace/project" }),
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
  return rootOnly
    ? JSON.parse(readFileSync(join(dirname(target), "root_session"), "utf8"))
    : readFileSync(target, "utf8").trim();
}
const rootOnly = await publish(process.argv[2], true);
const defaultMode = await publish(process.argv[3], false);
process.stdout.write(JSON.stringify({ rootOnly, defaultMode }));
"#;
        let output = std::process::Command::new("node")
            .args(["--input-type=module", "--eval", script])
            .arg(store.join("extensions/aoe-session-id.js"))
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
        assert_eq!(published["rootOnly"]["id"], parent_id);
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
            published["rootOnly"]["path"],
            store
                .canonicalize()
                .unwrap()
                .join("custom-sessions/parent.jsonl")
                .to_str()
                .unwrap()
        );

        publish_root(child_id, "child.jsonl");
        assert_eq!(
            inst.prime_root_publication(),
            None,
            "a direct child transcript must fail root validation"
        );
        publish_root(parent_id, "parent.jsonl");

        let mut restarted: Instance =
            serde_json::from_str(&serde_json::to_string(&inst).unwrap()).unwrap();
        let mut command = "prime-agent".to_string();
        assert!(restarted.apply_session_flags(&mut command, "test").unwrap());
        assert_eq!(command, format!("prime-agent --resume {parent_id}"));

        let profile = restarted.effective_profile();
        let storage = crate::session::storage::Storage::new_unwatched(&profile).unwrap();
        storage
            .update(|instances, _| {
                instances.push(restarted.clone());
                Ok(())
            })
            .unwrap();
        let prepared = restarted.prepare_launch_command().unwrap();
        assert!(prepared.command.as_deref().unwrap().contains(parent_id));
        let excluded_prepared = restarted.prepare_launch_command().unwrap();
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
        publish_root(newer_id, "newer.jsonl");
        for stored in [None, Some(parent_id.to_string())] {
            let mut excluded = restarted.clone();
            excluded.agent_session_id = stored.clone();
            excluded
                .retroactive_capture_excludes
                .insert(newer_id.to_string());
            let mut command = "prime-agent".to_string();
            excluded.apply_session_flags(&mut command, "test").unwrap();
            assert_eq!(excluded.agent_session_id, stored);
            assert!(!command.contains(newer_id), "{command}");
        }
        let mut excluded = restarted.clone();
        excluded
            .retroactive_capture_excludes
            .insert(newer_id.to_string());
        let excluded_launch = excluded
            .refresh_prepared_prime_launch_after_pane_stop(excluded_prepared)
            .unwrap();
        assert_eq!(excluded.agent_session_id.as_deref(), Some(parent_id));
        assert!(excluded_launch
            .command
            .as_deref()
            .unwrap()
            .contains(parent_id));
        assert!(!excluded_launch
            .command
            .as_deref()
            .unwrap()
            .contains(newer_id));

        let prepared = restarted
            .refresh_prepared_prime_launch_after_pane_stop(prepared)
            .unwrap();
        assert_eq!(restarted.agent_session_id.as_deref(), Some(newer_id));
        assert!(prepared.command.as_deref().unwrap().contains(newer_id));
        let _ = restarted.persist_session_id(
            &profile,
            prepared.expected_prior_sid.as_deref(),
            prepared.expected_prior_intent.clone(),
        );
        assert_eq!(
            storage.load().unwrap()[0].agent_session_id.as_deref(),
            Some(newer_id)
        );
        assert_eq!(restarted.agent_session_id.as_deref(), Some(newer_id));
        storage
            .update(|instances, _| {
                instances[0].agent_session_id = Some(child_id.to_string());
                Ok(())
            })
            .unwrap();
        let _ = restarted.persist_session_id(
            &profile,
            prepared.expected_prior_sid.as_deref(),
            prepared.expected_prior_intent,
        );
        assert_eq!(
            storage.load().unwrap()[0].agent_session_id.as_deref(),
            Some(child_id)
        );
        assert_eq!(restarted.agent_session_id.as_deref(), Some(child_id));
    }
}
