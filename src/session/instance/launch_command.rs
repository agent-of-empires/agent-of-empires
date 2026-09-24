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
    pub(super) expected_conversation: ConversationState,
    pub(super) canonical_conversation: Option<ConversationState>,
    pub(super) expected_prior_omp_generation: Option<String>,
    pub(super) execution: Option<super::execution::NativeExecution>,
    /// A known conversation was physically relocated before this launch; the
    /// finalize step must confirm the durable row carries the relocated state
    /// before the restart can claim success.
    pub(super) carry_relocated: bool,
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
/// Restore cwd and native routing after the login shell's startup files.
pub(super) fn wrap_command_ignore_suspend(
    cmd: &str,
    working_dir: &str,
    routing: &[(String, Option<String>)],
    case_insensitive_routing: &[&str],
) -> String {
    let user = crate::session::environment::user_shell();
    let posix = crate::session::environment::user_posix_shell();
    let mut script = execution_context(working_dir, routing, case_insensitive_routing);
    script.push_str(&format!("stty susp undef\nexec env {cmd}"));
    shell_stdin_command(&posix, user == posix, &script, "AOE_LAUNCH_BODY")
}

fn execution_context(
    working_dir: &str,
    routing: &[(String, Option<String>)],
    case_insensitive_routing: &[&str],
) -> String {
    let mut script = format!(
        "cd {} || exit 1\n",
        crate::session::environment::shell_escape_script_word(working_dir)
    );
    if !case_insensitive_routing.is_empty() {
        script.push_str(
            "eval \"$(unset aoe_key aoe_value || { printf 'exit 1\\n'; exit 1; }\nset | while IFS='=' read -r aoe_key aoe_value; do\ncase \"$aoe_key\" in\n",
        );
        let mut retained = routing
            .iter()
            .filter(|(key, value)| {
                value.is_some()
                    && case_insensitive_routing
                        .iter()
                        .any(|name| key.eq_ignore_ascii_case(name))
            })
            .peekable();
        if retained.peek().is_some() {
            for (index, (key, _)) in retained.enumerate() {
                if index != 0 {
                    script.push('|');
                }
                script.push_str(key);
            }
            script.push_str(") ;;\n");
        }
        for (index, key) in case_insensitive_routing.iter().enumerate() {
            if index != 0 {
                script.push('|');
            }
            for byte in key.bytes() {
                if byte.is_ascii_alphabetic() {
                    script.push('[');
                    script.push(byte.to_ascii_lowercase() as char);
                    script.push(byte.to_ascii_uppercase() as char);
                    script.push(']');
                } else {
                    script.push(byte as char);
                }
            }
        }
        script
            .push_str(") printf 'unset %s || exit 1\\n' \"$aoe_key\";;\nesac\ndone)\" || exit 1\n");
    }
    for (key, value) in routing {
        match value {
            Some(value) => {
                let value = crate::session::environment::shell_escape_script_word(value);
                script.push_str(&format!(
                    "if [ \"${{{key}+x}}\" = x ] && [ \"${key}\" = {value} ]; then\nexport {key} || exit 1\nelse\nexport {key}={value} || exit 1\nfi\n"
                ));
            }
            None => script.push_str(&format!("unset {key} || exit 1\n")),
        }
    }
    script
}

fn wrap_native_container_command(
    cmd: &str,
    execution: Option<&super::execution::NativeExecution>,
) -> Result<String> {
    let Some(execution) = execution else {
        return Ok(cmd.to_owned());
    };
    let mut script = execution_context(
        execution
            .inputs
            .cwd
            .to_str()
            .context("native cwd is not UTF-8")?,
        &execution.routing,
        execution.case_insensitive_routing,
    );
    script.push_str(&format!("exec env {cmd}"));
    Ok(format!(
        "/bin/sh -c {}",
        crate::session::environment::shell_escape_script_word(&script)
    ))
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

    pub(super) fn prepare_launch_command(
        &mut self,
        expected_conversation: ConversationState,
    ) -> Result<PreparedLaunch> {
        let expected_prior_omp_generation = self.omp_capture_generation.clone();
        let prior_probe_failed_sid = self.resume_probe_failed_sid.clone();
        let preparation = (|| -> Result<_> {
            if matches!(self.resume_intent, ResumeIntent::Default) {
                if let Some(observation) = self.capture_freshest_conversation() {
                    self.apply_conversation_observation(&observation);
                }
            }
            let validate_target = !matches!(self.resume_intent, ResumeIntent::Default)
                || self
                    .conversation_target()
                    .and_then(|(_, binding, _)| binding)
                    .is_some_and(ConversationBinding::is_known);
            let managed = !matches!(self.resume_intent, ResumeIntent::Cleared)
                && (self.agent_session_id.is_some()
                    || matches!(
                        self.resume_intent,
                        ResumeIntent::Use(_) | ResumeIntent::Fork { .. }
                    ));
            let execution = match self.resolve_native_execution(self.conversation_target()) {
                Ok(execution) => {
                    if validate_target {
                        self.validate_conversation_target(
                            &execution.binding,
                            execution.target_session_id.as_deref(),
                        )?;
                    }
                    Some(execution)
                }
                Err(error) if managed && !matches!(self.resume_intent, ResumeIntent::Default) => {
                    return Err(error);
                }
                Err(error) => {
                    tracing::debug!(target: "session.store", error = %error, "native execution unavailable; using native launch flags");
                    None
                }
            };
            let prior_canonical = if let Some(target) = execution
                .as_ref()
                .and_then(|execution| execution.resolved_target_session_id.as_ref())
            {
                let prior = self.conversation_state();
                let mut binding = self
                    .conversation_target()
                    .and_then(|(_, binding, _)| binding)
                    .context("resolved Hermes target has no validated binding")?
                    .clone();
                binding.session_id.clone_from(target);
                if matches!(self.resume_intent, ResumeIntent::Use(_)) {
                    self.resume_intent = ResumeIntent::Use(target.clone());
                    self.resume_binding = Some(binding.clone());
                }
                self.set_agent_conversation(
                    Some(target.clone()),
                    Some(binding),
                    self.pi_session_path.clone(),
                );
                Some(prior)
            } else {
                None
            };
            let parts = self.build_launch_command(execution.as_ref())?;
            if (managed || parts.1) && validate_target {
                if let Some(execution) = execution.as_ref() {
                    self.validate_conversation_target(
                        &execution.binding,
                        execution
                            .resolved_target_session_id
                            .as_deref()
                            .or(execution.target_session_id.as_deref()),
                    )?;
                }
            }
            let canonical_conversation = prior_canonical.map(|prior| {
                let canonical = self.conversation_state();
                self.adopt_conversation_state(prior);
                canonical
            });
            Ok((parts, execution, canonical_conversation))
        })();
        let (
            (command, is_existing, omp_capture_plan, mut launch_env),
            mut execution,
            canonical_conversation,
        ) = match preparation {
            Ok(prepared) => prepared,
            Err(error) => {
                self.adopt_conversation_state(expected_conversation);
                self.resume_probe_failed_sid = prior_probe_failed_sid;
                self.omp_capture_generation = expected_prior_omp_generation;
                return Err(error);
            }
        };
        if let Some(execution) = execution.as_mut() {
            launch_env.pane = std::mem::take(&mut execution.inputs.pane_env);
            launch_env.container = execution
                .inputs
                .docker_env
                .take()
                .map(|environment| environment.env)
                .unwrap_or_default();
        }
        if omp_capture_plan.is_some() && !self.is_sandboxed() {
            launch_env.pane.extend(omp_host_routing_environment(
                &self.resolved_host_environment(),
            ));
        }
        Ok(PreparedLaunch {
            command,
            is_existing,
            omp_capture_plan,
            launch_env,
            expected_conversation,
            canonical_conversation,
            expected_prior_omp_generation,
            execution,
            carry_relocated: false,
        })
    }

    /// Refresh after pane teardown; Prime resident workers may still be running.
    pub(super) fn refresh_prepared_prime_launch_after_pane_stop(
        &mut self,
        prepared: PreparedLaunch,
    ) -> Result<PreparedLaunch> {
        if !prepared.is_existing {
            self.set_agent_conversation(
                prepared.expected_conversation.session_id.clone(),
                prepared.expected_conversation.binding.clone(),
                prepared.expected_conversation.pi_session_path.clone(),
            );
        }
        self.absorb_published_prime_session();
        let mut refreshed = self.prepare_launch_command(prepared.expected_conversation)?;
        refreshed.expected_prior_omp_generation = prepared.expected_prior_omp_generation;
        Ok(refreshed)
    }

    /// Construct the command only after hook execution has completed. Keeping this phase hook-free
    /// prevents a revalidation retry from replaying user code while the lifecycle lock is held.
    pub(super) fn build_launch_command(
        &mut self,
        execution: Option<&super::execution::NativeExecution>,
    ) -> Result<LaunchCommandParts> {
        if self.tool == "omp" && !self.has_command_override() {
            reject_omp_secret_args(&crate::session::config::quote_model_value_in_args(
                &self.extra_args,
            ))?;
        }
        let agent = execution
            .map(|execution| execution.agent)
            .or_else(|| self.default_selector_agent());

        let (cmd, is_existing, omp_capture_plan, launch_env) = if self.is_sandboxed() {
            let image = self
                .sandbox_info
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("sandbox_info missing for sandboxed instance"))?
                .image
                .clone();
            let fallback_container = execution
                .is_none()
                .then(|| DockerContainer::new(&self.id, &image));
            let snapshot = execution.and_then(|execution| execution.inputs.container.as_ref());
            anyhow::ensure!(
                execution.is_none() || snapshot.is_some(),
                "prepared sandbox transport is missing"
            );

            let omp_capture_plan = execution
                .and_then(|execution| execution.omp.as_ref())
                .and_then(|context| {
                    self.resolve_omp_capture_plan(
                        context,
                        snapshot.map(|snapshot| snapshot.runtime.kind),
                    )
                });

            let launch_cmd = self.freeze_native_invocation(self.get_launch_command(), execution)?;
            let base_cmd = if self.extra_args.is_empty() {
                launch_cmd
            } else if self.command.is_empty() {
                // Default agent binary: quote a shell-active --model/-m value
                // the same way the host launch path does (build_host_command).
                // A custom command override is the user's own argv, so it is
                // left untouched, matching that path's scoping.
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

            let extension_backend = agent
                .and_then(|agent| agent.session_support.as_ref())
                .and_then(|support| support.capture.as_ref())
                .map(|capture| capture.backend);
            let fallback_identity = execution
                .is_none()
                .then(|| self.identity_extension_launch())
                .flatten();
            let identity_extension = execution
                .and_then(|execution| execution.inputs.identity_extension.as_ref())
                .or(fallback_identity.as_ref());
            let extension_configured = identity_extension.is_some();
            self.pi_extension_launched = extension_configured
                && extension_backend == Some(crate::agents::SessionCaptureBackend::Pi);
            if let Some((ref flag, _)) = identity_extension {
                tool_cmd.push_str(flag);
            }
            let is_existing =
                self.apply_session_flags(&mut tool_cmd, "sandboxed", agent, execution)?;
            apply_agent_launch_env(&mut tool_cmd, agent);

            let fallback_environment = if execution.is_none() {
                Some(self.sandbox_launch_environment(
                    agent,
                    identity_extension,
                    &self.effective_profile(),
                    None,
                    &crate::session::config::repo_config::resolve_config_with_repo(
                        &self.effective_profile(),
                        std::path::Path::new(&self.project_path),
                    )?,
                )?)
            } else {
                None
            };
            let env_info = execution
                .and_then(|execution| execution.inputs.docker_env.as_ref())
                .or(fallback_environment.as_ref())
                .context("prepared sandbox environment is missing")?;
            let env_part = format!("{} ", env_info.docker_args);
            let exec_command = |cmd: &str| match snapshot {
                Some(snapshot) => {
                    snapshot
                        .runtime
                        .exec_shell_command(&snapshot.id, Some(&env_part), cmd)
                }
                None => fallback_container
                    .as_ref()
                    .expect("unmanaged sandbox runtime")
                    .exec_command(Some(&env_part), cmd),
            };
            let raw_command = exec_command(&wrap_native_container_command(&tool_cmd, execution)?);
            let launch_command = if let Some(plan) = omp_capture_plan.as_ref() {
                let marked_tool_cmd = wrap_omp_launch(&tool_cmd, plan);
                let marked_command =
                    exec_command(&wrap_native_container_command(&marked_tool_cmd, execution)?);
                gate_omp_launch(&raw_command, &marked_command, plan)
            } else {
                raw_command
            };
            let (runtime_cwd, runtime_routing) = match snapshot {
                Some(snapshot) => (
                    snapshot
                        .runtime
                        .cwd
                        .to_str()
                        .context("runtime cwd is not UTF-8")?,
                    snapshot.runtime.routing.as_slice(),
                ),
                None => (self.project_path.as_str(), &[][..]),
            };
            let wrapped =
                wrap_command_ignore_suspend(&launch_command, runtime_cwd, runtime_routing, &[]);
            (
                Some(wrapped),
                is_existing,
                omp_capture_plan,
                LaunchEnvironment {
                    pane: Vec::new(),
                    container: fallback_environment
                        .map(|environment| environment.env)
                        .unwrap_or_default(),
                },
            )
        } else {
            let result = self.build_host_command(agent, execution)?;
            let env = if execution.is_none() {
                crate::session::environment::resolve_host_environment_pairs(
                    &self.resolved_host_environment(),
                )
                .into_iter()
                .map(|(key, value)| tmux::PaneEnvMutation::set(key, value))
                .collect()
            } else {
                Vec::new()
            };
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
        execution: Option<&super::execution::NativeExecution>,
    ) -> Result<(Option<String>, bool, Option<OmpCapturePlan>)> {
        let fallback_identity = execution
            .is_none()
            .then(|| self.identity_extension_launch())
            .flatten();
        let identity_extension = execution
            .and_then(|execution| execution.inputs.identity_extension.as_ref())
            .or(fallback_identity.as_ref());
        self.build_host_command_with_identity_extension(agent, identity_extension, execution)
    }

    fn build_host_command_with_identity_extension(
        &mut self,
        agent: Option<&'static crate::agents::AgentDef>,
        identity_extension: Option<&(String, String)>,
        execution: Option<&super::execution::NativeExecution>,
    ) -> Result<(Option<String>, bool, Option<OmpCapturePlan>)> {
        let omp_capture_plan = execution
            .and_then(|execution| execution.omp.as_ref())
            .and_then(|context| self.resolve_omp_capture_plan(context, None));

        let fallback_profile;
        let profile = if let Some(execution) = execution {
            execution.inputs.profile.as_str()
        } else {
            fallback_profile = self.effective_profile();
            &fallback_profile
        };
        let mut env_prefix = status_hook_env_prefix(profile, &self.id, self.status_agent());
        // The publisher is pane-scoped, including for safe Default wrappers.
        self.pi_extension_launched = false;
        if let Some((_, ref env)) = identity_extension {
            env_prefix.push_str(env);
            self.pi_extension_launched = true;
            env_prefix.push_str("AOE_SESSION_ROOT_ONLY=0 ");
        }
        let env_prefix = env_prefix;

        if self.command.is_empty() {
            match agent {
                Some(a) => {
                    let mut cmd =
                        self.freeze_native_invocation(a.launch_base_command(), execution)?;
                    if let Some((ref flag, _)) = identity_extension {
                        cmd.push_str(flag);
                    }
                    if !self.extra_args.is_empty() {
                        // A model id carrying shell metacharacters (a
                        // context-window suffix such as `[1m]`) would abort the
                        // launch line before the agent starts.
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
                    let is_existing =
                        self.apply_session_flags(&mut cmd, "host agent", agent, execution)?;
                    apply_agent_launch_env(&mut cmd, agent);
                    let raw_command = format!("{}{}", env_prefix, cmd);
                    let command = if let Some(plan) = omp_capture_plan.as_ref() {
                        let marked_command = wrap_omp_host_launch(&env_prefix, &cmd, plan);
                        gate_omp_launch(&raw_command, &marked_command, plan)
                    } else {
                        raw_command
                    };
                    Ok((
                        Some(wrap_command_ignore_suspend(
                            &command,
                            &self.project_path,
                            execution.map_or(&[], |execution| execution.routing.as_slice()),
                            execution.map_or(&[], |execution| execution.case_insensitive_routing),
                        )),
                        is_existing,
                        omp_capture_plan,
                    ))
                }
                None => Ok((None, false, omp_capture_plan)),
            }
        } else {
            let mut cmd = self.freeze_native_invocation(self.command.clone(), execution)?;
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
            let is_existing =
                self.apply_session_flags(&mut cmd, "host custom", agent, execution)?;
            apply_agent_launch_env(&mut cmd, agent);
            let raw_command = format!("{}{}", env_prefix, cmd);
            let command = if let Some(plan) = omp_capture_plan.as_ref() {
                let marked_command = wrap_omp_host_launch(&env_prefix, &cmd, plan);
                gate_omp_launch(&raw_command, &marked_command, plan)
            } else {
                raw_command
            };
            Ok((
                Some(wrap_command_ignore_suspend(
                    &command,
                    &self.project_path,
                    execution.map_or(&[], |execution| execution.routing.as_slice()),
                    execution.map_or(&[], |execution| execution.case_insensitive_routing),
                )),
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
        inst.build_host_command(agent, None).unwrap().0.unwrap()
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
        let identity_extension = (
            " -e '/tmp/pi-aoe-session-id.js'".to_string(),
            "AOE_SESSION_ID_FILE='/tmp/pi-session-id' ".to_string(),
        );
        let (command, _, _) = alias
            .build_host_command_with_identity_extension(agent, Some(&identity_extension), None)
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
        let command = wrapper.build_host_command(agent, None).unwrap().0.unwrap();
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
        let wrapped = wrap_command_ignore_suspend(&cmd, "/tmp/proj", &[], &[]);
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
            let wrapped = wrap_command_ignore_suspend("claude", "/tmp/proj", &[], &[]);
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
        let wrapped = wrap_command_ignore_suspend("pwd", working_dir.to_str().unwrap(), &[], &[]);
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
            inst.apply_session_flags(&mut cmd, "test", crate::agents::get_agent(tool), None)
                .unwrap();
            assert_eq!(cmd, expected);
        }
    }

    #[test]
    fn resume_command_uses_validated_executable_anchor() {
        let home = tempfile::tempdir().unwrap();
        let _isolation = crate::session::test_support::isolate_app_dir_at(home.path());
        let profile = "validated-wrapper-anchor";
        crate::session::instance::test_helpers::declare_execution_aliases(
            profile,
            &[("codex-personal", "codex")],
            home.path(),
        );
        let cases = [
            (
                "tab separator",
                "codex",
                "",
                "codex\t--model o3",
                "codex resume SID\t--model o3",
            ),
            (
                "leading whitespace",
                "codex",
                "",
                " \tcodex\t--model o3",
                " \tcodex resume SID\t--model o3",
            ),
            (
                "multiple spaces",
                "codex",
                "",
                "codex   --model o3",
                "codex resume SID   --model o3",
            ),
            (
                "direct alias",
                "codex-personal",
                "codex",
                " \tcodex\t--model o3",
                " \tcodex resume SID\t--model o3",
            ),
        ];
        for (name, tool, detect_as, command, expected) in cases {
            let mut inst = Instance::new(name, "/tmp/x");
            inst.tool = tool.to_string();
            inst.detect_as = detect_as.to_string();
            inst.command = command.to_string();
            inst.agent_session_id = Some("SID".to_string());
            inst.resume_intent = ResumeIntent::Use("SID".to_string());
            let mut cmd = command.to_string();

            assert!(
                inst.apply_session_flags(&mut cmd, "test", inst.resolved_agent(), None)
                    .unwrap(),
                "{name}"
            );
            assert_eq!(cmd, expected, "{name}");
            assert_eq!(
                shell_words::split(&cmd).unwrap(),
                ["codex", "resume", "SID", "--model", "o3"],
                "{name}"
            );
        }

        let mut wrapper = Instance::new("wrapper", "/tmp/x");
        wrapper.source_profile = profile.into();
        wrapper.tool = "codex-personal".to_string();
        wrapper.detect_as = "codex".to_string();
        wrapper.command = "codex-personal".to_string();
        wrapper.agent_session_id = Some("SID".to_string());
        wrapper.resume_intent = ResumeIntent::Use("SID".to_string());
        let mut cmd = wrapper.command.clone();

        assert!(wrapper
            .apply_session_flags(&mut cmd, "test", wrapper.resolved_agent(), None)
            .unwrap());
        assert_eq!(cmd, "codex-personal resume SID");

        // A launcher still hides the binary, so the token would reach `ssh`.
        let mut launcher = Instance::new("launcher", "/tmp/x");
        launcher.source_profile = profile.into();
        launcher.tool = "codex-personal".to_string();
        launcher.detect_as = "codex".to_string();
        launcher.command = "ssh -t host codex".to_string();
        launcher.agent_session_id = Some("SID".to_string());
        launcher.resume_intent = ResumeIntent::Use("SID".to_string());
        let mut launcher_cmd = launcher.command.clone();

        assert!(launcher
            .apply_session_flags(&mut launcher_cmd, "test", launcher.resolved_agent(), None)
            .is_err());
        assert_eq!(launcher_cmd, "ssh -t host codex");
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
    #[test]
    #[serial_test::serial]
    fn default_known_target_attempts_moved_context_without_rebinding() {
        let root = tempfile::tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(root.path());
        let _env = crate::session::test_support::EnvGuard::set(&[
            ("HOME", root.path().to_str().unwrap()),
            ("CODEX_HOME", root.path().join("codex-a").to_str().unwrap()),
        ]);
        let _claude = crate::session::test_support::install_login_shell_path_command(
            root.path(),
            "claude",
            "#!/bin/sh\nexit 1\n",
        );
        let _codex = crate::session::test_support::install_login_shell_path_command(
            root.path(),
            "codex",
            "#!/bin/sh\nexit 1\n",
        );
        for name in ["codex-a", "codex-b"] {
            let home = root.path().join(name);
            std::fs::create_dir_all(&home).unwrap();
            std::fs::write(home.join("auth.json"), r#"{"OPENAI_API_KEY":"test"}"#).unwrap();
        }
        let sid = "11111111-2222-4333-8444-555555555555";
        for agent in ["claude", "codex"] {
            let before = root.path().join(format!("{agent}-before"));
            let after = root.path().join(format!("{agent}-after"));
            std::fs::create_dir_all(&before).unwrap();
            std::fs::create_dir_all(&after).unwrap();
            let mut inst = tool_instance(agent, before.to_str().unwrap());
            inst.command = agent.into();
            let asserted = inst.asserted_resume_binding(sid, None);
            if agent == "codex" && crate::process::HAS_CODEX_MANAGED_PREFERENCES {
                assert_eq!(
                    asserted.unwrap_err().to_string(),
                    "Codex managed preferences cannot be attested by the local file contract"
                );
                continue;
            }
            let known = asserted.unwrap();
            inst.set_agent_conversation(Some(sid.into()), Some(known.clone()), None);
            if agent == "claude" {
                inst.project_path = after.to_str().unwrap().into();
            }
            let _changed = (agent == "codex").then(|| {
                crate::session::test_support::EnvGuard::set(&[(
                    "CODEX_HOME",
                    root.path().join("codex-b").to_str().unwrap(),
                )])
            });
            let prepared = inst
                .prepare_launch_command(inst.conversation_state())
                .unwrap();
            let flag = if agent == "codex" {
                "resume "
            } else {
                "--resume "
            };
            assert!(prepared.command.unwrap().contains(&format!("{flag}{sid}")));
            assert_eq!(inst.agent_session_binding.as_ref(), Some(&known));
            for intent in [
                ResumeIntent::Use(sid.into()),
                ResumeIntent::Fork { from: sid.into() },
            ] {
                inst.resume_intent = intent;
                inst.resume_binding = Some(known.clone());
                assert!(inst
                    .prepare_launch_command(inst.conversation_state())
                    .is_err());
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn sandboxed_assertion_keeps_the_native_container_store() {
        let temp = tempfile::tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(&temp.path().join("app"));
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let profile = "sandbox-asserted-store";
        super::super::test_helpers::declare_execution_aliases(
            profile,
            &[("claude", "claude")],
            temp.path(),
        );
        let mut inst = Instance::new("claude-sandbox", project.to_str().unwrap());
        inst.tool = "claude".into();
        inst.command = "claude".into();
        inst.source_profile = profile.into();
        inst.sandbox_info = Some(crate::session::SandboxInfo {
            enabled: true,
            container_id: None,
            image: "fixture".into(),
            container_name: "claude-sandbox".into(),
            extra_env: None,
            custom_instruction: None,
            container_workdir: Some("/workspace/project".into()),
            before_start_env: Vec::new(),
        });
        let config = inst.build_container_config().unwrap();
        let _transport = super::super::test_helpers::install_container_transport(
            temp.path(),
            "claude-sandbox",
            &config.volumes,
        );
        std::fs::copy(
            temp.path().join("native-bin/prime-agent"),
            temp.path().join("native-bin/claude"),
        )
        .unwrap();
        let sid = "11111111-1111-4111-8111-111111111111";
        let execution = inst.resolve_native_execution(None).unwrap();
        let asserted = inst.asserted_resume_binding(sid, None).unwrap();
        inst.resume_intent = ResumeIntent::Use(sid.into());
        inst.resume_binding = Some(asserted);
        inst.prepare_launch_command(inst.conversation_state())
            .unwrap();
        let resumed = inst
            .resolve_native_execution(inst.conversation_target())
            .unwrap();
        inst.validate_conversation_target(&resumed.binding, Some(sid))
            .unwrap();
        assert_eq!(resumed.binding, execution.binding);
        assert_ne!(
            resumed.binding.stores[0],
            std::path::Path::new("/root/.claude")
        );
        assert_eq!(
            resumed
                .routing
                .iter()
                .find(|(key, _)| key == "CLAUDE_CONFIG_DIR")
                .unwrap()
                .1
                .as_deref(),
            Some("/root/.claude")
        );
    }
}
