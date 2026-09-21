//! Building the shell command a session launches with.

use super::*;

pub(super) type LaunchCommandParts = (
    Option<String>,
    bool,
    Option<OmpCapturePlan>,
    LaunchEnvironment,
);

pub(super) struct LaunchEnvironment {
    pub(super) pane: Vec<tmux::PaneEnvMutation>,
    pub(super) container: Vec<(String, String)>,
}

pub(super) struct PreparedLaunch {
    pub(super) command: Option<String>,
    pub(super) is_existing: bool,
    pub(super) omp_capture_plan: Option<OmpCapturePlan>,
    pub(super) launch_env: LaunchEnvironment,
    pub(super) expected_prior_sid: Option<String>,
    pub(super) expected_prior_intent: ResumeIntent,
    pub(super) expected_prior_omp_generation: Option<String>,
}

/// Append yolo-mode flags or environment variables to a launch command.
fn apply_yolo_mode(cmd: &mut String, yolo: &crate::agents::YoloMode, is_sandboxed: bool) {
    match yolo {
        crate::agents::YoloMode::CliFlag(flag) => {
            *cmd = format!("{} {}", cmd, flag);
        }
        crate::agents::YoloMode::EnvVar(key, value) if !is_sandboxed => {
            *cmd = format_env_var_prefix(key, value, cmd);
        }
        crate::agents::YoloMode::EnvVar(..) | crate::agents::YoloMode::AlwaysYolo => {}
    }
}

/// Write the Pi session-id extension into the app dir and return its path.
pub(super) fn session_identity_extension_path() -> Result<PathBuf> {
    const SOURCE: &str = crate::session::instance::SESSION_IDENTITY_EXTENSION;
    let root = crate::session::get_app_dir()?;
    let rel = Path::new("agent-extensions").join("pi-aoe-session-id.js");
    let path = root.join(&rel);
    if std::fs::read_to_string(&path).ok().as_deref() != Some(SOURCE) {
        crate::session::replace_file_no_follow(&root, &rel, SOURCE.as_bytes())?;
    }
    Ok(path)
}

/// Whether a host `environment` list assigns `PATH`.
pub(super) fn environment_defines_path(environment: &[String]) -> bool {
    environment.iter().any(|entry| {
        entry
            .split_once('=')
            .is_some_and(|(key, _)| key.trim() == "PATH")
    })
}

pub(super) fn build_resume_flags(
    tool: &str,
    session_id: &str,
    is_existing_session: bool,
) -> String {
    use crate::agents::{get_agent, ResumeStrategy};

    if !is_valid_session_id(session_id) {
        tracing::warn!(target: "session.store",
            "Refusing to build resume flags: invalid session ID {:?}",
            session_id
        );
        return String::new();
    }
    let Some(agent) = get_agent(tool) else {
        return String::new();
    };
    let Some(support) = agent.session_support.as_ref() else {
        tracing::info!(target: "session.store",
            tool = %tool,
            sid = %session_id,
            "session resume is disabled for this agent; stored ID left unused"
        );
        return String::new();
    };
    match &support.resume {
        ResumeStrategy::Flag(flag) => format!("{} {}", flag, session_id),
        ResumeStrategy::FlagPair {
            existing,
            new_session,
        } => {
            let flag = if is_existing_session {
                existing
            } else {
                new_session
            };
            format!("{} {}", flag, session_id)
        }
        ResumeStrategy::Subcommand(sub) => format!("{} {}", sub, session_id),
    }
}

/// Build the launch flags for a one-shot terminal fork. Returns the empty string for an unforkable
/// agent or an invalid id (mirroring `build_resume_flags`'s fail-closed contract).
pub(super) fn build_fork_flags(tool: &str, parent_id: &str, child_id: &str) -> String {
    use crate::agents::{get_agent, ForkStrategy, ResumeStrategy};

    if !is_valid_session_id(parent_id) || !is_valid_session_id(child_id) {
        tracing::warn!(target: "session.store",
            "Refusing to build fork flags: invalid id (parent={parent_id:?} child={child_id:?})");
        return String::new();
    }
    let Some(agent) = get_agent(tool) else {
        return String::new();
    };
    match agent.fork_strategy {
        ForkStrategy::ClaudeFork => {
            format!("--resume {parent_id} --fork-session --session-id {child_id}")
        }
        ForkStrategy::CodexFork => {
            // Codex mints its own forked id; child_id is unused. The subcommand
            // is inserted after the binary by apply_session_flags.
            format!("fork {parent_id}")
        }
        ForkStrategy::Flag(fork_flag) => {
            // Resume the parent session (using the agent's own resume flag),
            // then add the fork flag; the agent mints the new id.
            match agent.session_support.as_ref().map(|support| support.resume) {
                Some(ResumeStrategy::Flag(resume_flag)) => {
                    format!("{resume_flag} {parent_id} {fork_flag}")
                }
                _ => String::new(),
            }
        }
        ForkStrategy::Unsupported => String::new(),
    }
}

pub(super) struct ParsedLaunchCommand {
    pub(super) words: Vec<String>,
    pub(super) executable_end: usize,
}

pub(super) fn parse_launch_command(command: &str) -> Option<ParsedLaunchCommand> {
    let words = shell_words::split(command).ok()?;
    words.first()?;

    let mut started = false;
    let mut quote = None;
    let mut escaped = false;
    for (offset, ch) in command.char_indices() {
        let separator = matches!(ch, ' ' | '\t' | '\n' | '\r');
        if !started {
            if separator {
                continue;
            }
            started = true;
        }
        if escaped {
            escaped = false;
            continue;
        }
        match quote {
            Some('\'') => {
                if ch == '\'' {
                    quote = None;
                }
            }
            Some('"') => match ch {
                '"' => quote = None,
                '\\' => escaped = true,
                _ => {}
            },
            _ => match ch {
                '\'' | '"' => quote = Some(ch),
                '\\' => escaped = true,
                _ if separator => {
                    return Some(ParsedLaunchCommand {
                        words,
                        executable_end: offset,
                    });
                }
                _ => {}
            },
        }
    }
    Some(ParsedLaunchCommand {
        words,
        executable_end: command.len(),
    })
}

/// Insert a subcommand at the parsed executable boundary, or append flags.
pub(super) fn splice_subcommand_or_append(
    cmd: &mut String,
    part: &str,
    subcommand_at: Option<usize>,
) {
    cmd.reserve(part.len() + 1);
    if let Some(offset) = subcommand_at {
        cmd.insert_str(offset, part);
        cmd.insert(offset, ' ');
    } else {
        cmd.push(' ');
        cmd.push_str(part);
    }
}

pub(super) fn append_resume_flags(
    tool: &str,
    session_id: Option<&str>,
    is_existing_session: bool,
    cmd: &mut String,
    executable_end: usize,
    context: &str,
) -> bool {
    use crate::agents::{get_agent, ResumeStrategy};

    if let Some(session_id) = session_id {
        let resume_part = build_resume_flags(tool, session_id, is_existing_session);
        if resume_part.is_empty() {
            return false;
        }
        let subcommand_at = matches!(
            get_agent(tool).and_then(|agent| agent.session_support.as_ref()),
            Some(crate::agents::SessionSupport {
                resume: ResumeStrategy::Subcommand(_),
                ..
            })
        )
        .then_some(executable_end);
        splice_subcommand_or_append(cmd, &resume_part, subcommand_at);
        tracing::debug!(target: "session.store", "Added resume flags to {} command: {}", context, resume_part);
        return true;
    }
    false
}

/// Format an environment variable assignment as a shell-safe command prefix.
fn format_env_var_prefix(key: &str, value: &str, cmd: &str) -> String {
    let escaped = shell_escape(value);
    format!("{}={} {}", key, escaped, cmd)
}

/// Prepend agent-specific environment overrides to a launch command.
fn apply_agent_launch_env(cmd: &mut String, agent: Option<&'static crate::agents::AgentDef>) {
    if !matches!(agent.map(|a| a.name), Some("antigravity" | "codex")) {
        return;
    }

    *cmd = format!(
        "env -u NO_COLOR TERM=xterm-256color COLORTERM=truecolor {}",
        cmd
    );
}

/// Run a script through a dedicated descriptor so its size is not constrained by the per-argument
/// exec limit and the launched agent retains the pane TTY on standard input.
pub(super) fn shell_stdin_command(shell: &str, login: bool, script: &str, stem: &str) -> String {
    let mut delimiter = stem.to_string();
    while script.lines().any(|line| line == delimiter) {
        delimiter.push('_');
    }
    let flag = if login { "-l " } else { "" };
    format!(
        "{} {flag}/dev/fd/3 3<<'{delimiter}'\n{script}\n{delimiter}",
        shell_escape(shell)
    )
}

/// Disable terminal suspension before replacing the pane process with the requested command.
pub(super) fn wrap_command_ignore_suspend(cmd: &str, working_dir: &str) -> String {
    let user = crate::session::environment::user_shell();
    let posix = crate::session::environment::user_posix_shell();
    let cd = crate::session::environment::shell_escape(working_dir);
    let script = format!("cd {cd} || exit 1\nstty susp undef\nexec env {cmd}");
    shell_stdin_command(&posix, user == posix, &script, "AOE_LAUNCH_BODY")
}

impl Instance {
    pub fn has_custom_command(&self) -> bool {
        if !self.extra_args.is_empty() {
            return true;
        }
        self.has_command_override()
    }

    /// True only when the launch command differs from the agent's default binary (ignores
    /// extra_args).
    pub fn has_command_override(&self) -> bool {
        if self.command.is_empty() {
            return false;
        }
        crate::agents::get_agent(&self.tool)
            .map(|a| self.command != a.binary)
            .unwrap_or(true)
    }

    pub fn expects_shell(&self) -> bool {
        crate::tmux::utils::is_shell_command(self.get_tool_command())
    }

    pub fn get_tool_command(&self) -> &str {
        if self.command.is_empty() {
            crate::agents::get_agent(&self.tool)
                .map(|a| a.binary)
                .unwrap_or("bash")
        } else {
            &self.command
        }
    }

    /// The text searched for a user-selected `--agent NAME` flag.
    pub(super) fn selected_agent_args(&self) -> String {
        if self.command.is_empty() {
            self.extra_args.clone()
        } else if self.extra_args.is_empty() {
            self.command.clone()
        } else {
            format!("{} {}", self.command, self.extra_args)
        }
    }

    /// Launch command including any agent `launch_subcommand` (e.g. `kiro-cli chat`).
    fn get_launch_command(&self) -> String {
        if self.command.is_empty() {
            crate::agents::get_agent(&self.tool)
                .map(|a| a.launch_base_command())
                .unwrap_or_else(|| "bash".to_string())
        } else {
            self.command.clone()
        }
    }

    pub(super) fn prepare_launch_command(&mut self) -> Result<PreparedLaunch> {
        let expected_prior_sid = self.agent_session_id.clone();
        let expected_prior_intent = self.resume_intent.clone();
        let expected_prior_omp_generation = self.omp_capture_generation.clone();
        let (command, is_existing, omp_capture_plan, launch_env) = self.build_launch_command()?;
        Ok(PreparedLaunch {
            command,
            is_existing,
            omp_capture_plan,
            launch_env,
            expected_prior_sid,
            expected_prior_intent,
            expected_prior_omp_generation,
        })
    }

    /// Refresh after pane teardown; Prime resident workers may still be running.
    pub(super) fn refresh_prepared_prime_launch_after_pane_stop(
        &mut self,
        mut prepared: PreparedLaunch,
    ) -> Result<PreparedLaunch> {
        if self.absorb_published_prime_session() {
            // Refresh launch data without changing the durable CAS baseline.
            (
                prepared.command,
                prepared.is_existing,
                prepared.omp_capture_plan,
                prepared.launch_env,
            ) = self.build_launch_command()?;
        }
        Ok(prepared)
    }

    /// Construct the command only after hook execution has completed. Keeping this phase hook-free
    /// prevents a revalidation retry from replaying user code while the lifecycle lock is held.
    pub(super) fn build_launch_command(&mut self) -> Result<LaunchCommandParts> {
        if self.tool == "omp" && !self.has_command_override() {
            reject_omp_secret_args(&crate::session::config::quote_model_value_in_args(
                &self.extra_args,
            ))?;
        }
        let agent = self.resolved_agent();
        let detect_as = self.effective_detect_as().into_owned();

        let (cmd, is_existing, omp_capture_plan, launch_env) = if self.is_sandboxed() {
            let image = self
                .sandbox_info
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("sandbox_info missing for sandboxed instance"))?
                .image
                .clone();
            let container = DockerContainer::new(&self.id, &image);

            // Snapshot only after container hooks have had their final chance to mutate OMP
            // dotenv/config routing, but before any executable pane command exists.
            let omp_capture_plan = self
                .omp_capture_options()
                .and_then(|options| self.resolve_omp_capture_plan(&options));

            let launch_cmd = self.get_launch_command();
            let base_cmd = if self.extra_args.is_empty() {
                launch_cmd
            } else if self.command.is_empty() {
                // Default agent binary: quote a shell-active --model/-m value the same way the host
                // launch path does (build_host_command).
                format!(
                    "{} {}",
                    launch_cmd,
                    crate::session::config::quote_model_value_in_args(&self.extra_args)
                )
            } else {
                format!("{} {}", launch_cmd, self.extra_args)
            };
            let mut tool_cmd = if self.is_yolo_mode() {
                if let Some(ref yolo) = agent.and_then(|a| a.yolo.as_ref()) {
                    match yolo {
                        crate::agents::YoloMode::CliFlag(flag) => {
                            format!("{} {}", base_cmd, flag)
                        }
                        crate::agents::YoloMode::EnvVar(..)
                        | crate::agents::YoloMode::AlwaysYolo => base_cmd,
                    }
                } else {
                    base_cmd
                }
            } else {
                base_cmd
            };
            if let Some(instruction) = self
                .sandbox_info
                .as_ref()
                .and_then(|s| s.custom_instruction.as_ref())
                .filter(|s| !s.is_empty())
            {
                if let Some(flag_template) = agent.and_then(|a| a.instruction_flag) {
                    let escaped = shell_escape(instruction);
                    let flag = flag_template.replace("{}", &escaped);
                    tool_cmd = format!("{} {}", tool_cmd, flag);
                }
            }

            let extension_backend = self.resolved_capture_backend();
            let identity_extension = self.identity_extension_launch();
            let extension_configured = identity_extension.is_some();
            self.pi_extension_launched = extension_configured
                && extension_backend == Some(crate::agents::SessionCaptureBackend::Pi);
            if let Some((ref flag, _)) = identity_extension {
                tool_cmd.push_str(flag);
            }
            let is_existing = self.apply_session_flags(&mut tool_cmd, "sandboxed")?;
            apply_agent_launch_env(&mut tool_cmd, agent);

            let sandbox = self
                .sandbox_info
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("sandbox_info missing for sandboxed instance"))?;
            let managed_codex_home = container_config::managed_codex_home(
                &self.tool,
                Some(detect_as.as_str()),
                &self.source_profile,
                &self.id,
            )?;
            let mut env_info = build_docker_env_args_with_managed_codex_home(
                &self.source_profile,
                sandbox,
                std::path::Path::new(&self.project_path),
                managed_codex_home.as_deref(),
            );
            let profile = self.effective_profile();
            if !env_info.docker_args.is_empty() {
                env_info.docker_args.push(' ');
            }
            env_info.docker_args.push_str(&format!(
                "-e AOE_PROFILE={} -e AOE_INSTANCE_ID={}",
                shell_escape(&profile),
                shell_escape(&self.id)
            ));
            if let Some(agent) = agent {
                env_info
                    .docker_args
                    .push_str(&format!(" -e AOE_AGENT_BIN={}", shell_escape(agent.binary)));
            }
            if let Some(&(key, expected)) = agent.and_then(|agent| {
                agent
                    .container_env
                    .iter()
                    .find(|(key, _)| *key == "PRIME_AGENT_CODING_AGENT_DIR")
            }) {
                env_info
                    .docker_args
                    .push_str(&format!(" -e {key}={}", shell_escape(expected)));
            }
            if let Some((_, ref env)) = identity_extension {
                // `KEY=VALUE ` from the host form, passed as a docker `-e`.
                env_info
                    .docker_args
                    .push_str(&format!(" -e {}", shell_escape(env.trim())));
            }
            if extension_configured {
                let root_only = matches!(
                    extension_backend.and_then(|backend| backend.identity_publisher()),
                    Some(crate::agents::SessionIdentityPublisher::Extension { root_only: true })
                );
                env_info.docker_args.push_str(if root_only {
                    " -e AOE_SESSION_ROOT_ONLY=1"
                } else {
                    " -e AOE_SESSION_ROOT_ONLY=0"
                });
            }
            let env_part = format!("{} ", env_info.docker_args);
            let raw_command = container.exec_command(Some(&env_part), &tool_cmd);
            let launch_command = if let Some(plan) = omp_capture_plan.as_ref() {
                let marked_tool_cmd = wrap_omp_launch(&tool_cmd, plan);
                let marked_command = container.exec_command(Some(&env_part), &marked_tool_cmd);
                gate_omp_launch(&raw_command, &marked_command, plan)
            } else {
                raw_command
            };
            let wrapped = wrap_command_ignore_suspend(&launch_command, &self.project_path);
            (
                Some(wrapped),
                is_existing,
                omp_capture_plan,
                LaunchEnvironment {
                    pane: Vec::new(),
                    container: env_info.env,
                },
            )
        } else {
            let result = self.build_host_command(agent)?;
            let mut env = crate::session::environment::resolve_host_environment_pairs(
                &self.profile_host_environment(),
            )
            .into_iter()
            .map(|(key, value)| tmux::PaneEnvMutation::set(key, value))
            .collect::<Vec<_>>();
            // The protected file is sourced in order, so freshly minted hook
            // values appended last override same-keyed static profile values.
            env.extend(
                self.pending_host_env
                    .iter()
                    .cloned()
                    .map(|(key, value)| tmux::PaneEnvMutation::set(key, value)),
            );
            if result.2.is_some() {
                // Pin every routing input, including explicit empty values and true absence, so
                // tmux's frozen server environment cannot select another OMP store.
                env.extend(omp_host_routing_environment(
                    &self.resolved_host_environment(),
                ));
            }
            (
                result.0,
                result.1,
                result.2,
                LaunchEnvironment {
                    pane: env,
                    container: Vec::new(),
                },
            )
        };

        Ok((cmd, is_existing, omp_capture_plan, launch_env))
    }

    /// Build the tmux command for a host session after all launch hooks have
    /// completed.
    fn build_host_command(
        &mut self,
        agent: Option<&'static crate::agents::AgentDef>,
    ) -> Result<(Option<String>, bool, Option<OmpCapturePlan>)> {
        let identity_extension = self.identity_extension_launch();
        self.build_host_command_with_identity_extension(agent, identity_extension)
    }

    fn build_host_command_with_identity_extension(
        &mut self,
        agent: Option<&'static crate::agents::AgentDef>,
        identity_extension: Option<(String, String)>,
    ) -> Result<(Option<String>, bool, Option<OmpCapturePlan>)> {
        // Resolve after `on_launch`. The snapshot is checked inside the profile environment
        // assignment scope executed by the login shell.
        let omp_capture_plan = self
            .omp_capture_options()
            .and_then(|options| self.resolve_omp_capture_plan(&options));

        let profile = self.effective_profile();
        let mut env_prefix = status_hook_env_prefix(&profile, &self.id, agent);
        // A verified direct Pi command publishes through the same extension
        // whether it came from the built-in command or an exact alias.
        self.pi_extension_launched = false;
        if let Some((_, ref env)) = identity_extension {
            env_prefix.push_str(env);
            self.pi_extension_launched = true;
            env_prefix.push_str("AOE_SESSION_ROOT_ONLY=0 ");
        }
        let env_prefix = env_prefix;

        if self.command.is_empty() {
            match crate::agents::get_agent(&self.tool) {
                Some(a) => {
                    let mut cmd = a.launch_base_command();
                    if let Some((ref flag, _)) = identity_extension {
                        cmd.push_str(flag);
                    }
                    if !self.extra_args.is_empty() {
                        // A model id carrying shell metacharacters (a context-window suffix such as
                        // `[1m]`) would abort the launch line before the agent starts.
                        cmd = format!(
                            "{} {}",
                            cmd,
                            crate::session::config::quote_model_value_in_args(&self.extra_args)
                        );
                    }
                    if self.is_yolo_mode() {
                        if let Some(ref yolo) = a.yolo {
                            apply_yolo_mode(&mut cmd, yolo, false);
                        }
                    }
                    let is_existing = self.apply_session_flags(&mut cmd, "host agent")?;
                    apply_agent_launch_env(&mut cmd, agent);
                    let raw_command = format!("{}{}", env_prefix, cmd);
                    let command = if let Some(plan) = omp_capture_plan.as_ref() {
                        let marked_command = wrap_omp_host_launch(&env_prefix, &cmd, plan);
                        gate_omp_launch(&raw_command, &marked_command, plan)
                    } else {
                        raw_command
                    };
                    Ok((
                        Some(wrap_command_ignore_suspend(&command, &self.project_path)),
                        is_existing,
                        omp_capture_plan,
                    ))
                }
                None => Ok((None, false, omp_capture_plan)),
            }
        } else {
            let mut cmd = self.command.clone();
            if let Some((ref flag, _)) = identity_extension {
                cmd.push_str(flag);
            }
            if !self.extra_args.is_empty() {
                cmd = format!("{} {}", cmd, self.extra_args);
            }
            if self.is_yolo_mode() {
                if let Some(yolo) = agent.and_then(|a| a.yolo.as_ref()) {
                    apply_yolo_mode(&mut cmd, yolo, false);
                }
            }
            let is_existing = self.apply_session_flags(&mut cmd, "host custom")?;
            apply_agent_launch_env(&mut cmd, agent);
            let raw_command = format!("{}{}", env_prefix, cmd);
            let command = if let Some(plan) = omp_capture_plan.as_ref() {
                let marked_command = wrap_omp_host_launch(&env_prefix, &cmd, plan);
                gate_omp_launch(&raw_command, &marked_command, plan)
            } else {
                raw_command
            };
            Ok((
                Some(wrap_command_ignore_suspend(&command, &self.project_path)),
                is_existing,
                omp_capture_plan,
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::instance::test_helpers::*;
    use crate::session::test_support::EnvGuard;

    fn host_command(inst: &mut Instance) -> String {
        let agent = crate::agents::get_agent(&inst.tool);
        inst.build_host_command(agent).unwrap().0.unwrap()
    }

    // The sidecar env var has to survive into the docker argv; no CI container would catch it.
    #[test]
    #[serial_test::serial]
    fn sandboxed_pi_publishes_through_env_without_a_command_line_extension() {
        let (_guard, _base, _tmp) = crate::hooks::test_support::BaseGuard::ready();
        let temp_home = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_home(temp_home.path());
        let project = temp_home.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let mut inst = tool_instance("pi", project.to_str().unwrap());
        let mut sandbox = test_sandbox("aoe-pi-argv", Some("/workspace"));
        sandbox.extra_env = Some(vec!["AOE_SESSION_ROOT_ONLY=1".to_string()]);
        inst.sandbox_info = Some(sandbox);
        let sidecar = format!(
            "AOE_SESSION_ID_FILE={}/{}/session_id",
            crate::session::config::container_config::PI_SIDECAR_DIR_IN_CONTAINER,
            inst.id
        );

        // `pi -e <missing path>` refuses to start, so a sandboxed launch names no extension.
        let (flag, env) = inst
            .identity_extension_launch()
            .expect("sandboxed pi publishes");
        assert!(flag.is_empty());
        assert_eq!(env.trim(), sidecar);

        let (cmd, _, _, _) = inst
            .build_launch_command()
            .expect("a sandboxed launch line");
        let cmd = cmd.expect("a command");
        assert!(cmd.contains(&sidecar), "{cmd}");
        assert!(
            !cmd.contains(" -e /") || !cmd.contains("aoe-session-id.js"),
            "{cmd}"
        );
        assert!(cmd.contains(" -e AOE_SESSION_ROOT_ONLY=0"), "{cmd}");
    }

    #[test]
    #[serial_test::serial]
    fn pi_extension_is_injected_only_into_a_direct_pi_launch() {
        let home = tempfile::tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(home.path());

        let mut alias = Instance::new("pi alias", "/tmp/pi-alias-launch");
        alias.tool = "company-pi".to_string();
        alias.detect_as = "pi".to_string();
        alias.command = "pi".to_string();
        let agent = alias.resolved_agent();
        let (command, _, _) = alias
            .build_host_command_with_identity_extension(
                agent,
                Some((
                    " -e '/tmp/pi-aoe-session-id.js'".to_string(),
                    "AOE_SESSION_ID_FILE='/tmp/pi-session-id' ".to_string(),
                )),
            )
            .unwrap();
        let command = command.unwrap();
        assert!(command.contains(" -e "), "{command}");
        assert!(command.contains("AOE_SESSION_ID_FILE="));
        assert!(command.contains("AOE_SESSION_ROOT_ONLY=0"));
        assert!(alias.pi_extension_launched);

        let mut wrapper = Instance::new("pi alias wrapper", "/tmp/pi-alias-wrapper");
        wrapper.tool = "company-pi".to_string();
        wrapper.detect_as = "pi".to_string();
        wrapper.command = "echo not-pi".to_string();
        assert!(wrapper.identity_extension_launch().is_none());
        let agent = wrapper.resolved_agent();
        let command = wrapper.build_host_command(agent).unwrap().0.unwrap();
        assert!(!command.contains("pi-aoe-session-id.js"));
        assert!(!command.contains("AOE_SESSION_ID_FILE="));
        assert!(!wrapper.pi_extension_launched);

        let mut terminated = tool_instance("pi", "/tmp/pi-terminator");
        terminated.extra_args = "--".to_string();
        let command = host_command(&mut terminated);
        assert!(!command.contains(" -e "), "extension follows --: {command}");
        assert!(!terminated.pi_extension_launched);
    }

    #[test]
    fn every_agent_has_yolo_support() {
        for agent in crate::agents::AGENTS {
            assert!(agent.yolo.is_some(), "{}", agent.name);
        }
    }

    #[test]
    fn yolo_envvar_value_is_quoted_and_survives_the_suspend_wrapper() {
        let cmd = format_env_var_prefix("OPENCODE_PERMISSION", r#"{"*":"allow"}"#, "opencode");
        assert_eq!(cmd, r#"OPENCODE_PERMISSION='{"*":"allow"}' opencode"#);
        let wrapped = wrap_command_ignore_suspend(&cmd, "/tmp/proj");
        assert!(wrapped.contains(r#"OPENCODE_PERMISSION='{"*":"allow"}' opencode"#));
    }

    #[test]
    #[serial_test::serial(shell_env)]
    fn wrap_command_runs_a_descriptor_script_login_only_for_posix_shells() {
        for (shell, prefix) in [
            ("/bin/bash", ""),
            ("/bin/zsh", "'/bin/zsh' -l /dev/fd/3 "),
            // fish and nu PATH setup is not in bash login files.
            ("/usr/bin/fish", "'bash' /dev/fd/3 "),
            ("/usr/bin/nu", "'bash' /dev/fd/3 "),
        ] {
            let _shell = EnvGuard::set(&[("SHELL", shell)]);
            let wrapped = wrap_command_ignore_suspend("claude", "/tmp/proj");
            assert!(wrapped.starts_with(prefix), "{shell}: {wrapped}");
            assert!(wrapped.contains("/dev/fd/3 3<<'AOE_LAUNCH_BODY'"));
            assert!(wrapped.contains("\nstty susp undef\nexec env claude\n"));
            assert!(!wrapped.contains(" -c "));
        }
    }

    /// Login shell rc files can `cd` after tmux set the pane cwd, so the script re-enters it.
    #[test]
    fn wrap_command_reasserts_working_dir_after_login_shell() {
        let _lock = EnvGuard::read_lock();
        let Ok(bash) = which::which("bash") else {
            eprintln!("skipping: bash not found on PATH");
            return;
        };
        let _shell = EnvGuard::set(&[("SHELL", &bash)]);
        let temp = tempfile::tempdir().unwrap();
        let working_dir = temp.path().join("some project's dir");
        std::fs::create_dir(&working_dir).unwrap();
        let wrapped = wrap_command_ignore_suspend("pwd", working_dir.to_str().unwrap());
        assert!(wrapped.contains("3<<'AOE_LAUNCH_BODY'\ncd "), "{wrapped}");
        assert!(wrapped.contains("|| exit 1\nstty susp undef"), "{wrapped}");
        let output = std::process::Command::new(&bash)
            .args(["-c", &wrapped])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let printed = String::from_utf8_lossy(&output.stdout);
        assert_eq!(
            std::path::Path::new(printed.trim()).canonicalize().unwrap(),
            working_dir.canonicalize().unwrap(),
        );
    }

    #[test]
    fn tool_command_and_custom_command_detection() {
        // (tool, command, extra_args, tool command, has_custom_command, has_command_override,
        //  expects_shell)
        for (tool, command, extra, want_cmd, custom, overridden, shell) in [
            ("claude", "", "", "claude", false, false, false),
            ("opencode", "", "", "opencode", false, false, false),
            ("codex", "", "", "codex", false, false, false),
            ("gemini", "", "", "gemini", false, false, false),
            ("unknown", "", "", "bash", false, false, true),
            (
                "claude",
                "claude --resume abc123",
                "",
                "claude --resume abc123",
                true,
                true,
                false,
            ),
            ("claude", "claude", "", "claude", false, false, false),
            ("claude", "my-wrapper", "", "my-wrapper", true, true, false),
            ("claude", "bash", "", "bash", true, true, true),
            (
                "unknown_agent",
                "unknown_agent",
                "",
                "unknown_agent",
                true,
                true,
                false,
            ),
            ("claude", "", "--model opus", "claude", true, false, false),
        ] {
            let mut inst = tool_instance(tool, "/tmp/test");
            inst.command = command.to_string();
            inst.extra_args = extra.to_string();
            let label = format!("{tool}/{command}/{extra}");
            assert_eq!(inst.get_tool_command(), want_cmd, "{label}");
            assert_eq!(inst.has_custom_command(), custom, "{label}");
            assert_eq!(inst.has_command_override(), overridden, "{label}");
            assert_eq!(inst.expects_shell(), shell, "{label}");
        }
    }

    #[test]
    fn resume_and_fork_flags_per_agent() {
        let sid = "019342ab-1234-7def-8901-abcdef012345";
        for (tool, existing, expected) in [
            ("claude", true, format!("--resume {sid}")),
            ("claude", false, format!("--session-id {sid}")),
            ("opencode", true, format!("--session {sid}")),
            ("opencode", false, format!("--session {sid}")),
            ("vibe", true, format!("--resume {sid}")),
            ("copilot", true, format!("--session-id {sid}")),
            ("pi", true, format!("--session {sid}")),
            ("pi", false, format!("--session-id {sid}")),
            ("mistral", false, String::new()),
        ] {
            assert_eq!(
                build_resume_flags(tool, sid, existing),
                expected,
                "{tool}/{existing}"
            );
        }
        assert_eq!(build_resume_flags("claude", "$(rm -rf /)", true), "");
        assert_eq!(build_resume_flags("opencode", "id; echo pwned", false), "");

        for (tool, parent, child, expected) in [
            ("codex", "parent-id", "ignored-child", "fork parent-id"),
            (
                "opencode",
                "parent-id",
                "ignored-child",
                "--session parent-id --fork",
            ),
            ("cursor", "parent", "child", ""),
            ("claude", "$(rm -rf /)", "child", ""),
            ("claude", "parent", "; echo pwned", ""),
        ] {
            assert_eq!(build_fork_flags(tool, parent, child), expected, "{tool}");
        }
    }

    #[test]
    fn fork_command_places_codex_subcommand_after_binary_and_appends_flags() {
        for (tool, cmd, expected) in [
            (
                "codex",
                "codex --some-flag",
                "codex fork parent-1234 --some-flag",
            ),
            (
                "opencode",
                "opencode",
                "opencode --session parent-1234 --fork",
            ),
        ] {
            let mut inst = tool_instance(tool, "/tmp/x");
            inst.agent_session_id = Some("child-ignored".to_string());
            inst.resume_intent = ResumeIntent::Fork {
                from: "parent-1234".to_string(),
            };
            let mut cmd = cmd.to_string();
            inst.apply_session_flags(&mut cmd, "test").unwrap();
            assert_eq!(cmd, expected);
        }
    }

    #[test]
    fn resume_command_uses_validated_executable_anchor() {
        // (tool, detect_as, command, expected, resumed)
        for (tool, detect_as, command, expected, resumed) in [
            (
                "codex",
                "",
                "codex\t--model o3",
                "codex resume SID\t--model o3",
                true,
            ),
            (
                "codex",
                "",
                " \tcodex\t--model o3",
                " \tcodex resume SID\t--model o3",
                true,
            ),
            (
                "codex",
                "",
                "codex   --model o3",
                "codex resume SID   --model o3",
                true,
            ),
            (
                "codex-personal",
                "codex",
                " \tcodex\t--model o3",
                " \tcodex resume SID\t--model o3",
                true,
            ),
            // A bare renamed wrapper is the program the pane runs.
            (
                "codex-personal",
                "codex",
                "codex-personal",
                "codex-personal resume SID",
                true,
            ),
            // A launcher hides the binary, so the token would reach `ssh`.
            (
                "codex-personal",
                "codex",
                "ssh -t host codex",
                "ssh -t host codex",
                false,
            ),
        ] {
            let mut inst = tool_instance(tool, "/tmp/x");
            inst.detect_as = detect_as.to_string();
            inst.command = command.to_string();
            inst.agent_session_id = Some("SID".to_string());
            inst.resume_intent = ResumeIntent::Use("SID".to_string());
            let mut cmd = command.to_string();
            assert_eq!(
                inst.apply_session_flags(&mut cmd, "test").unwrap(),
                resumed,
                "{command:?}"
            );
            assert_eq!(cmd, expected);
            if command.contains("--model") {
                assert_eq!(
                    shell_words::split(&cmd).unwrap(),
                    ["codex", "resume", "SID", "--model", "o3"]
                );
            }
        }
    }

    #[test]
    fn environment_defines_path_only_for_the_assigning_form() {
        // A pass-through entry keeps AoE's own PATH; an assignment can front a different pi.
        assert!(environment_defines_path(&["PATH=/opt/bin".to_string()]));
        assert!(environment_defines_path(&[
            "API_KEY=x".to_string(),
            " PATH =/opt/bin".to_string()
        ]));
        assert!(!environment_defines_path(&["PATH".to_string()]));
        assert!(!environment_defines_path(&["PATHOLOGICAL=1".to_string()]));
        assert!(!environment_defines_path(&[]));
    }

    #[test]
    fn host_command_applies_yolo_resume_and_launch_subcommand() {
        let mut codex = tool_instance("codex", "/tmp/test");
        codex.yolo_mode = true;
        let cmd = host_command(&mut codex);
        match crate::agents::get_agent("codex")
            .unwrap()
            .yolo
            .as_ref()
            .unwrap()
        {
            crate::agents::YoloMode::CliFlag(flag) => assert!(cmd.contains(flag)),
            crate::agents::YoloMode::EnvVar(key, _) => assert!(cmd.contains(key)),
            crate::agents::YoloMode::AlwaysYolo => {}
        }

        let mut claude = tool_instance("claude", "/tmp/test");
        claude.agent_session_id = Some("ses_abc123def456".to_string());
        let cmd = host_command(&mut claude);
        assert!(cmd.contains("ses_abc123def456"));
        assert!(cmd.contains("--session-id") || cmd.contains("--resume"));

        // Kiro must launch via `kiro-cli chat`, with yolo flags after the subcommand.
        let mut kiro = tool_instance("kiro", "/tmp/test");
        kiro.yolo_mode = true;
        let cmd = host_command(&mut kiro);
        let chat = cmd.find("kiro-cli chat").expect("chat subcommand present");
        assert!(cmd.find("--trust-all-tools").expect("yolo flag present") > chat);

        // A command override is verbatim: no injected subcommand.
        let mut custom = tool_instance("kiro", "/tmp/test");
        custom.command = "kiro-cli chat --trust-all-tools".to_string();
        assert_eq!(host_command(&mut custom).matches("chat").count(), 1);
    }

    #[test]
    fn host_command_forces_color_only_for_color_sensitive_agents() {
        for (tool, command, needle, forced) in [
            ("antigravity", "", "agy", true),
            ("antigravity", "agy --some-flag", "agy --some-flag", true),
            ("codex", "", "codex", true),
            ("cursor", "", "", false),
        ] {
            let mut inst = tool_instance(tool, "/tmp/test");
            inst.command = command.to_string();
            let cmd = host_command(&mut inst);
            assert!(cmd.contains(needle), "{cmd}");
            for env in [
                "env -u NO_COLOR",
                "TERM=xterm-256color",
                "COLORTERM=truecolor",
            ] {
                assert_eq!(cmd.contains(env), forced, "{tool}: {env}");
            }
        }
    }

    #[test]
    fn selected_agent_args_combines_command_and_extra_last_wins() {
        for (command, extra, expected) in [
            ("", "--agent custom-agent", "custom-agent"),
            ("kiro-cli chat --agent custom-agent", "", "custom-agent"),
            (
                "kiro-cli chat --agent from-command",
                "--agent from-extra",
                "from-extra",
            ),
        ] {
            let mut inst = tool_instance("kiro", "/tmp/test");
            inst.command = command.to_string();
            inst.extra_args = extra.to_string();
            assert_eq!(
                crate::agents::parse_selected_agent(&inst.selected_agent_args(), "--agent")
                    .as_deref(),
                Some(expected)
            );
        }
    }
}
