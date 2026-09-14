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
    pub(super) expected_prior_omp_generation: Option<String>,
    pub(super) execution: Option<super::execution::NativeExecution>,
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
///
/// Rewritten when the content differs so an upgrade ships its own version.
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

/// Whether a host `environment` list assigns `PATH`. Entries are either `KEY`
/// (pass AoE's own value through, which cannot redirect a binary lookup) or
/// `KEY=VALUE`, so only the assigning form counts.
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

/// Build the launch flags for a one-shot terminal fork. Returns the empty
/// string for an unforkable agent or an invalid id (mirroring
/// `build_resume_flags`'s fail-closed contract). The child id is pre-pinned so
/// the forked session is durable on disk before launch.
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
///
/// Uses `shell_escape` (single-quote escaping) so the value is preserved
/// verbatim when parsed by the inner `bash -c '...'` shell created by
/// `wrap_command_ignore_suspend`.
fn format_env_var_prefix(key: &str, value: &str, cmd: &str) -> String {
    let escaped = shell_escape(value);
    format!("{}={} {}", key, escaped, cmd)
}

/// Prepend agent-specific environment overrides to a launch command.
///
/// Some terminal agents inherit the parent tmux env, which can carry
/// `NO_COLOR=1` and silently disable their terminal palettes even though the
/// web renderer handles ANSI fine. Unsetting `NO_COLOR` and advertising
/// `TERM=xterm-256color` plus `COLORTERM=truecolor` at launch keeps color on
/// without pinning tools to a specific `FORCE_COLOR` depth.
fn apply_agent_launch_env(cmd: &mut String, agent: Option<&'static crate::agents::AgentDef>) {
    if !matches!(agent.map(|a| a.name), Some("antigravity" | "codex")) {
        return;
    }

    *cmd = format!(
        "env -u NO_COLOR TERM=xterm-256color COLORTERM=truecolor {}",
        cmd
    );
}

/// Run a script through a dedicated descriptor so its size is not constrained
/// by the per-argument exec limit and the launched agent retains the pane TTY
/// on standard input. The delimiter grows until it cannot close a here-document
/// present in user-controlled command text.
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

/// Disable terminal suspension before replacing the pane process with the
/// requested command. The user's POSIX login shell reads the launch script
/// from a dedicated descriptor, keeping both large prompts and the pane TTY.
///
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
            "eval \"$(set | while IFS='=' read -r aoe_key aoe_value; do\ncase \"$aoe_key\" in\n",
        );
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
        // Only fixed identifier patterns can emit an unset command.
        script.push_str(") printf 'unset %s\\n' \"$aoe_key\";;\nesac\ndone)\"\n");
    }
    for (key, value) in routing {
        match value {
            Some(value) => script.push_str(&format!(
                "export {key}={}\n",
                crate::session::environment::shell_escape_script_word(value)
            )),
            None => script.push_str(&format!("unset {key}\n")),
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

    /// True only when the launch command differs from the agent's default
    /// binary (ignores extra_args). Use this for status-detection and
    /// restart guards where only a wrapper script matters.
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

    /// The text searched for a user-selected `--agent NAME` flag: both the
    /// command override (where a custom command like `kiro-cli chat --agent x`
    /// may live) and the extra-args field (the usual place). Joined so a flag
    /// in either is found.
    pub(super) fn selected_agent_args(&self) -> String {
        if self.command.is_empty() {
            self.extra_args.clone()
        } else if self.extra_args.is_empty() {
            self.command.clone()
        } else {
            format!("{} {}", self.command, self.extra_args)
        }
    }

    /// Launch command including any agent `launch_subcommand` (e.g.
    /// `kiro-cli chat`). A user command override takes precedence verbatim and
    /// the subcommand is not applied to it. Used when assembling the launch
    /// command so subcommand-scoped flags (yolo, resume) parse correctly.
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
            let managed = !matches!(self.resume_intent, ResumeIntent::Cleared)
                && (self.agent_session_id.is_some()
                    || matches!(
                        self.resume_intent,
                        ResumeIntent::Use(_) | ResumeIntent::Fork { .. }
                    ));
            let execution = match self.resolve_native_execution(self.conversation_target()) {
                Ok(execution) => {
                    self.validate_conversation_target(
                        &execution.binding,
                        execution.target_session_id.as_deref(),
                    )?;
                    Some(execution)
                }
                Err(error) if managed => return Err(error),
                Err(_) => None,
            };
            let parts = self.build_launch_command(execution.as_ref())?;
            if managed || parts.1 {
                let execution = execution
                    .as_ref()
                    .context("conversation execution adapter is unavailable")?;
                self.validate_conversation_target(
                    &execution.binding,
                    execution.target_session_id.as_deref(),
                )?;
            }
            Ok((parts, execution))
        })();
        let ((command, is_existing, omp_capture_plan, mut launch_env), mut execution) =
            match preparation {
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
        Ok(PreparedLaunch {
            command,
            is_existing,
            omp_capture_plan,
            launch_env,
            expected_conversation,
            expected_prior_omp_generation,
            execution,
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

    /// Construct the command only after hook execution has completed. Keeping
    /// this phase hook-free prevents a revalidation retry from replaying user
    /// code while the lifecycle lock is held.
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
            .or_else(|| self.resolved_agent());

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
    #[test]
    #[serial_test::serial]
    fn opencode_preparation_refuses_workspace_forwarding() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let _isolation = crate::session::test_support::isolate_app_dir_at(home.path());
        let _environment = crate::session::test_support::EnvGuard::unset(&[
            "OPENCODE_CONFIG_CONTENT",
            "OPENCODE_WORKSPACE_ID",
        ]);
        let root = home.path().canonicalize().unwrap();
        let program = root.join("opencode");
        let recording = root.join("argv");
        std::fs::write(
            &program,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\n",
                shell_escape(recording.to_str().unwrap())
            ),
        )
        .unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let database = root.join("conversation.db");
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection.execute_batch("CREATE TABLE session (id TEXT PRIMARY KEY, workspace_id TEXT, directory TEXT NOT NULL)").unwrap();
        let parent = "ses_0123456789abcdef0123456789abcdef";
        connection
            .execute(
                "INSERT INTO session (id, directory) VALUES (?1, ?2)",
                [parent, root.to_str().unwrap()],
            )
            .unwrap();
        let mut child = Instance::new("opencode-routing", root.to_str().unwrap());
        child.tool = "opencode".into();
        child.command = "opencode".into();
        child.pending_host_env = vec![
            ("PATH".into(), format!("{}:/usr/bin:/bin", root.display())),
            ("HOME".into(), root.to_str().unwrap().into()),
            ("OPENCODE_DB".into(), database.to_str().unwrap().into()),
        ];
        let binding = child.resolve_native_execution(None).unwrap().binding;
        child.agent_session_id = Some("22222222-2222-4222-8222-222222222222".into());
        child.resume_intent = ResumeIntent::Fork {
            from: parent.into(),
        };
        child.resume_binding = Some(ConversationBinding {
            session_id: parent.into(),
            execution: Some(binding),
            provenance: ConversationProvenance::Observed,
            transcript_path: None,
        });
        let prepared = child
            .prepare_launch_command(child.conversation_state())
            .unwrap();
        assert!(std::process::Command::new("/bin/sh")
            .args(["-c", prepared.command.as_deref().unwrap()])
            .status()
            .unwrap()
            .success());
        assert_eq!(
            std::fs::read_to_string(&recording)
                .unwrap()
                .lines()
                .collect::<Vec<_>>(),
            vec!["--session", parent, "--fork"]
        );
        std::fs::remove_file(&recording).unwrap();
        connection
            .execute(
                "UPDATE session SET workspace_id = 'remote-workspace' WHERE id = ?1",
                [parent],
            )
            .unwrap();
        let expected = child.conversation_state();
        assert!(child.prepare_launch_command(expected.clone()).is_err());
        assert!(expected.matches(&child));
        assert!(!recording.exists());
    }

    #[test]
    #[serial_test::serial]
    #[serial_test::serial(hook_base)]
    fn preparation_adopts_qualified_publication_not_a_legacy_raw_id() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let _isolation = crate::session::test_support::isolate_app_dir_at(home.path());
        let (_hooks, _, _hook_dir) = crate::hooks::test_support::BaseGuard::ready();
        let program = home.path().join("claude");
        std::fs::write(&program, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let prior = "11111111-1111-4111-8111-111111111111";
        for published in [prior, "22222222-2222-4222-8222-222222222222"] {
            let mut instance =
                Instance::new("qualified-publication", home.path().to_str().unwrap());
            instance.tool = "claude".into();
            instance.command = "claude".into();
            instance.pending_host_env = vec![
                (
                    "PATH".into(),
                    format!("{}:/usr/bin:/bin", home.path().display()),
                ),
                ("HOME".into(), home.path().display().to_string()),
            ];
            let execution = instance
                .resolve_native_execution(instance.conversation_target())
                .unwrap();
            instance.set_agent_conversation(
                Some(prior.into()),
                Some(ConversationBinding {
                    session_id: prior.into(),
                    execution: Some(execution.binding.clone()),
                    provenance: ConversationProvenance::Preallocated,
                    transcript_path: None,
                }),
                None,
            );
            let directory = crate::hooks::ensure_instance_dir_path(&instance.id).unwrap();
            std::fs::write(directory.join("session_id"), "foreign-legacy-id").unwrap();
            std::fs::write(
                directory.join(format!("session_id.{}", execution.inputs.launch_id)),
                published,
            )
            .unwrap();
            instance.active_execution = Some(ActiveExecution {
                launch_id: execution.inputs.launch_id,
                binding: execution.binding,
                capture: execution.capture,
                container: None,
            });
            assert!(instance.fork_parent_binding().is_none());
            instance
                .prepare_launch_command(instance.conversation_state())
                .unwrap();
            assert_eq!(instance.agent_session_id.as_deref(), Some(published));
            assert_eq!(
                instance.fork_parent_binding().unwrap().session_id,
                published
            );
        }
    }

    #[test]
    #[serial_test::serial]
    fn fork_preparation_rejects_a_different_native_program() {
        let home = tempfile::tempdir().unwrap();
        let _isolation = crate::session::test_support::isolate_app_dir_at(home.path());
        use std::os::unix::fs::PermissionsExt;
        let program = home.path().join("claude");
        std::fs::write(&program, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let profile = "fork-binding";
        crate::session::instance::test_helpers::declare_execution_aliases(
            profile,
            &[("my-claude", "claude")],
            home.path(),
        );
        std::fs::copy(&program, home.path().join("my-claude")).unwrap();
        let mut child = Instance::new("fork-binding", home.path().to_str().unwrap());
        child.source_profile = profile.into();
        child.pending_host_env = vec![
            (
                "PATH".into(),
                format!("{}:/usr/bin:/bin", home.path().display()),
            ),
            ("HOME".into(), home.path().display().to_string()),
        ];
        child.tool = "claude".into();
        child.command = "claude".into();
        child.agent_session_id = Some("22222222-2222-4222-8222-222222222222".into());
        child.resume_intent = ResumeIntent::Fork {
            from: "11111111-1111-4111-8111-111111111111".into(),
        };
        child.resume_binding = Some(ConversationBinding {
            session_id: "11111111-1111-4111-8111-111111111111".into(),
            execution: Some(
                child
                    .resolve_native_execution(child.conversation_target())
                    .unwrap()
                    .binding,
            ),
            provenance: ConversationProvenance::Observed,
            transcript_path: None,
        });
        let mut incompatible = child.clone();
        incompatible.command = "codex".into();
        let mut aliased = child.clone();
        aliased.swap_tool("my-claude");
        aliased.command = "my-claude".into();
        let aliased = aliased
            .prepare_launch_command(aliased.conversation_state())
            .unwrap();
        assert!(aliased.command.unwrap().contains("--fork-session"));
        let mut unresolved_config = child.clone();
        let compatible = child
            .prepare_launch_command(child.conversation_state())
            .unwrap();
        assert!(compatible.command.unwrap().contains("--fork-session"));
        assert!(
            incompatible
                .prepare_launch_command(incompatible.conversation_state())
                .is_err(),
            "a Claude conversation must not dispatch Claude selectors to Codex"
        );
        std::fs::write(
            crate::session::config::profile_config::get_profile_config_path(profile).unwrap(),
            "[broken",
        )
        .unwrap();
        assert!(
            unresolved_config
                .prepare_launch_command(unresolved_config.conversation_state())
                .is_err(),
            "a managed fork must not substitute defaults for an unreadable execution configuration"
        );
    }

    #[test]
    #[serial_test::serial]
    fn prepared_hermes_sandbox_retains_post_launch_capture() {
        let temp = tempfile::tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(&temp.path().join("app"));
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let profile = "hermes-capture";
        super::super::test_helpers::declare_execution_aliases(
            profile,
            &[("hermes", "hermes")],
            temp.path(),
        );
        let mut inst = Instance::new("hermes-capture", project.to_str().unwrap());
        inst.tool = "hermes".into();
        inst.command = "hermes".into();
        inst.source_profile = profile.into();
        inst.sandbox_info = Some(crate::session::SandboxInfo {
            enabled: true,
            container_id: None,
            image: "fixture".into(),
            container_name: "hermes-capture".into(),
            extra_env: Some(vec!["HERMES_MANAGED_DIR=/root/.hermes/managed".into()]),
            custom_instruction: None,
            container_workdir: Some("/workspace/project".into()),
            before_start_env: Vec::new(),
        });
        let config = inst.build_container_config().unwrap();
        let _transport = super::super::test_helpers::install_container_transport(
            temp.path(),
            "hermes-capture",
            &config.volumes,
        );
        std::fs::copy(
            temp.path().join("native-bin/prime-agent"),
            temp.path().join("native-bin/hermes"),
        )
        .unwrap();
        let root = config
            .volumes
            .iter()
            .find(|mount| mount.container_path == "/root/.hermes")
            .unwrap()
            .host_path
            .clone();
        std::fs::create_dir_all(&root).unwrap();
        let database =
            rusqlite::Connection::open(std::path::Path::new(&root).join("state.db")).unwrap();
        database.execute_batch("CREATE TABLE sessions(id TEXT, source TEXT, started_at REAL, ended_at REAL, cwd TEXT, git_repo_root TEXT); INSERT INTO sessions VALUES ('stale', 'cli', 1000, NULL, '/workspace/project', NULL), ('foreign', 'cli', 4000, NULL, '/other', NULL), ('hermes_fresh', 'cli', 3000, NULL, '/workspace/project', NULL);").unwrap();
        let execution = inst.resolve_native_execution(None).unwrap();
        let binding = execution.binding.clone();
        inst.active_execution = Some(ActiveExecution {
            launch_id: execution.inputs.launch_id,
            binding: execution.binding,
            capture: execution.capture,
            container: execution.inputs.container,
        });
        let poll = crate::session::capture::hermes_poll_fn_sandboxed_store(
            inst.capture_store_dir()
                .expect("prepared sandbox must retain its capture source"),
            inst.container_workdir(),
            inst.id.clone(),
            std::time::UNIX_EPOCH + std::time::Duration::from_secs(2000),
            Default::default(),
            Some(binding),
        );
        assert_eq!(poll().as_deref(), Some("hermes_fresh"));
    }

    #[test]
    #[serial_test::serial]
    fn declared_resume_only_wrappers_keep_their_native_namespace() {
        let mut failures = Vec::new();
        for (agent, sid, redirect) in [
            (
                "vibe",
                "11111111-1111-4111-8111-111111111111",
                "VIBE_SESSION_LOGGING__SAVE_DIR=/foreign",
            ),
            (
                "hermes",
                "20260914_164000_a1b2c3",
                "export HERMES_HOME=/foreign",
            ),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let _app = crate::session::test_support::isolate_app_dir_at(temp.path());
            let wrapper = format!("my-{agent}");
            let payload = if agent == "hermes" {
                "printf 'namespace:%s|%s|%s|%s\\n' \"$HERMES_HOME\" \"$HERMES_MANAGED_DIR\" \"$TERMINAL_ENV\" \"$TERMINAL_CWD\""
            } else {
                "printf 'namespace:%s|%s|%s|%s|%s\\n' \"$VIBE_HOME\" \"$VIBE_SESSION_LOGGING__SAVE_DIR\" \"$vIbE_sEsSiOn_LoGgInG__sAvE_dIr\" \"$VIBE_SESSION_LOGGING__SESSION_PREFIX\" \"$vibe_session_logging__session_prefix\""
            };
            let _path = crate::session::test_support::install_login_shell_path_command(
                temp.path(),
                &wrapper,
                &format!("#!/bin/sh\nprintf '%s\\n' \"$@\"\n{payload}\n"),
            );
            let _namespace = EnvGuard::unset(&[
                "SAVE_DIR",
                "SESSION_PREFIX",
                "VIBE_AGENT_PATHS",
                "VIBE_DEFAULT_AGENT",
                "VIBE_SESSION_LOGGING",
                "VIBE_SESSION_LOGGING__SAVE_DIR",
                "VIBE_SESSION_LOGGING__SESSION_PREFIX",
                "vibe_session_logging__session_prefix",
            ]);
            let profile = "declared-native-resume";
            crate::session::instance::test_helpers::declare_execution_aliases(
                profile,
                &[(&wrapper, agent)],
                temp.path(),
            );
            let root = temp.path().join(format!(".{agent}"));
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(
                root.join(".env"),
                "API_KEY='local fixture'\nPRIVATE_KEY=\"first\nsecond\"\nVIBE_SESSION_LOGGING__GENERATE_TITLES=false\n",
            )
            .unwrap();
            let mut inst = Instance::new("declared-native", temp.path().to_str().unwrap());
            inst.tool = wrapper.clone();
            inst.command = wrapper;
            inst.source_profile = profile.into();
            inst.pending_host_env = vec![
                ("HOME".into(), temp.path().to_str().unwrap().into()),
                (
                    "HERMES_MANAGED_DIR".into(),
                    temp.path().join("managed").to_str().unwrap().into(),
                ),
                ("TERMINAL_ENV".into(), "local".into()),
                ("TERMINAL_CWD".into(), temp.path().to_str().unwrap().into()),
            ];
            if agent == "vibe" {
                inst.pending_host_env.push((
                    "vibe_session_logging__session_prefix".into(),
                    "session".into(),
                ));
            }
            let binding = match inst.asserted_resume_binding(sid, None) {
                Ok(binding) => binding,
                Err(error) => {
                    failures.push(format!("{agent}: {error:#}"));
                    continue;
                }
            };
            inst.resume_intent = ResumeIntent::Use(sid.into());
            inst.resume_binding = Some(binding);
            let mut redirected = inst.clone();
            let prepared = inst
                .prepare_launch_command(inst.conversation_state())
                .unwrap();
            let command = prepared.command.unwrap();
            let login = temp.path().join(".profile");
            let original_login = std::fs::read_to_string(&login).unwrap();
            std::fs::write(&login, format!("{original_login}\nexport HERMES_MANAGED_DIR=/foreign TERMINAL_ENV=ssh TERMINAL_CWD=/foreign VIBE_SESSION_LOGGING__SAVE_DIR=/foreign vIbE_sEsSiOn_LoGgInG__sAvE_dIr=/foreign VIBE_SESSION_LOGGING__SESSION_PREFIX=foreign vibe_session_logging__session_prefix=foreign\n")).unwrap();
            let output = std::process::Command::new("/bin/sh")
                .args(["-c", &command])
                .env("vIbE_sEsSiOn_LoGgInG__sAvE_dIr", "/tmux-foreign")
                .output()
                .unwrap();
            std::fs::write(&login, original_login).unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let expected = if agent == "hermes" {
                format!(
                    "--profile\ndefault\n--resume\n{sid}\nnamespace:{}|{}|local|{}\n",
                    root.display(),
                    temp.path().join("managed").display(),
                    temp.path().display()
                )
            } else {
                format!("--resume\n{sid}\nnamespace:{}||||session\n", root.display())
            };
            assert_eq!(String::from_utf8(output.stdout).unwrap(), expected);
            if agent == "hermes" {
                std::fs::write(
                    root.join("config.yaml"),
                    "secrets:\n  sources: [command]\n  command:\n    enabled: false\n",
                )
                .unwrap();
                redirected
                    .prepare_launch_command(redirected.conversation_state())
                    .unwrap();
                std::fs::write(
                    root.join("config.yaml"),
                    "secrets:\n  command:\n    enabled: true\n",
                )
                .unwrap();
                assert!(redirected
                    .prepare_launch_command(redirected.conversation_state())
                    .is_err());
                std::fs::remove_file(root.join("config.yaml")).unwrap();
            } else {
                let profile = root.join("agents/accept-edits.toml");
                std::fs::create_dir_all(profile.parent().unwrap()).unwrap();
                std::fs::write(&profile, "[session_logging]\nsave_dir = '/foreign'\n").unwrap();
                assert!(redirected
                    .prepare_launch_command(redirected.conversation_state())
                    .is_err());
                std::fs::remove_file(profile).unwrap();
            }
            std::fs::write(root.join(".env"), redirect).unwrap();
            let expected = redirected.conversation_state();
            assert!(redirected.prepare_launch_command(expected.clone()).is_err());
            assert!(expected.matches(&redirected));
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    #[serial_test::serial]
    fn failed_preparation_preserves_the_prior_conversation() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let _isolation = crate::session::test_support::isolate_app_dir_at(home.path());
        let program = home.path().join("claude");
        std::fs::write(&program, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut instance = Instance::new("rejected-resume", home.path().to_str().unwrap());
        instance.tool = "claude".into();
        instance.command = "claude".into();
        instance.pending_host_env = vec![
            (
                "PATH".into(),
                format!("{}:/usr/bin:/bin", home.path().display()),
            ),
            ("HOME".into(), home.path().display().to_string()),
        ];
        instance.agent_session_id = Some("prior-conversation".into());
        instance.resume_intent = ResumeIntent::Use("invalid target".into());
        instance.resume_binding = Some(ConversationBinding {
            session_id: "invalid target".into(),
            execution: Some(
                instance
                    .resolve_native_execution(instance.conversation_target())
                    .unwrap()
                    .binding,
            ),
            provenance: ConversationProvenance::Asserted,
            transcript_path: None,
        });
        let prior = instance.conversation_state();
        assert!(instance
            .prepare_launch_command(instance.conversation_state())
            .is_err());
        assert!(
            prior.matches(&instance),
            "a rejected launch must not replace the prior conversation"
        );
    }

    #[test]
    #[serial_test::serial]
    fn sandboxed_pi_publishes_without_a_command_line_extension() {
        // `pi -e <missing path>` refuses to start, and a container created
        // before this change has no mount for one, so a sandboxed launch names
        // no extension: pi discovers it inside the config bind instead. The
        // sidecar path it publishes to is a container path.
        //
        // The extension is written under `HOME`, so this owns one: the
        // global lock keeps it from racing another test's `HOME` swap.
        let temp_home = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::EnvGuard::set(&[("HOME", temp_home.path())]);

        let mut inst = Instance::new("pi-sandbox", "/tmp/pi-sandbox");
        inst.tool = "pi".to_string();
        inst.sandbox_info = Some(crate::session::SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test-image".to_string(),
            container_name: "aoe-pi-sandbox".to_string(),
            extra_env: None,
            custom_instruction: None,
            container_workdir: None,
            before_start_env: Vec::new(),
        });

        let (flag, env) = inst
            .identity_extension_launch()
            .expect("sandboxed pi publishes");
        assert!(flag.is_empty(), "no `-e` may reach a container launch");
        assert_eq!(
            env.trim(),
            format!(
                "AOE_SESSION_ID_FILE={}/{}/session_id",
                crate::session::config::container_config::PI_SIDECAR_DIR_IN_CONTAINER,
                inst.id
            )
        );
    }
    #[test]
    #[serial_test::serial]
    fn verified_direct_pi_alias_emits_the_extension_it_marks_as_launched() {
        let home = tempfile::tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(home.path());
        let mut inst = Instance::new("pi alias", "/tmp/pi-alias-launch");
        inst.tool = "company-pi".to_string();
        inst.detect_as = "pi".to_string();
        inst.command = "pi".to_string();
        let agent = inst.resolved_agent();

        let (command, _, _) = inst
            .build_host_command_with_identity_extension(
                agent,
                Some(&(
                    " -e '/tmp/pi-aoe-session-id.js'".to_string(),
                    "AOE_SESSION_ID_FILE='/tmp/pi-session-id' ".to_string(),
                )),
                None,
            )
            .unwrap();
        let command = command.unwrap();

        assert!(command.contains(" -e "), "missing Pi extension: {command}");
        assert!(command.contains("AOE_SESSION_ID_FILE="));
        assert!(command.contains("AOE_SESSION_ROOT_ONLY=0"));
        assert!(inst.pi_extension_launched);
    }

    #[test]
    #[serial_test::serial]
    fn pi_alias_with_non_pi_command_does_not_get_extension() {
        let home = tempfile::tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(home.path());
        let mut inst = Instance::new("pi alias wrapper", "/tmp/pi-alias-wrapper");
        inst.tool = "company-pi".to_string();
        inst.detect_as = "pi".to_string();
        inst.command = "echo not-pi".to_string();
        let agent = inst.resolved_agent();

        assert!(inst.identity_extension_launch().is_none());
        let (command, _, _) = inst.build_host_command(agent, None).unwrap();
        let command = command.unwrap();

        assert!(!command.contains("pi-aoe-session-id.js"));
        assert!(!command.contains("AOE_SESSION_ID_FILE="));
        assert!(!inst.pi_extension_launched);
    }

    #[test]
    #[serial_test::serial]
    fn pi_option_terminator_disables_extension_injection() {
        let home = tempfile::tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(home.path());
        let mut inst = Instance::new("pi terminator", "/tmp/pi-terminator");
        inst.tool = "pi".to_string();
        inst.extra_args = "--".to_string();
        let agent = inst.resolved_agent();

        let (command, _, _) = inst.build_host_command(agent, None).unwrap();
        let command = command.unwrap();

        assert!(!command.contains(" -e "), "extension follows --: {command}");
        assert!(!inst.pi_extension_launched);
    }

    use super::*;

    use crate::session::test_support::EnvGuard;

    #[test]
    fn test_all_agents_have_yolo_support() {
        for agent in crate::agents::AGENTS {
            assert!(
                agent.yolo.is_some(),
                "Agent '{}' should have YOLO mode configured",
                agent.name
            );
        }
    }

    #[test]
    fn test_yolo_mode_helper() {
        let mut inst = Instance::new("test", "/tmp/test");
        assert!(!inst.is_yolo_mode());

        inst.yolo_mode = true;
        assert!(inst.is_yolo_mode());

        inst.yolo_mode = false;
        assert!(!inst.is_yolo_mode());
    }

    #[test]
    fn test_yolo_mode_without_sandbox() {
        let mut inst = Instance::new("test", "/tmp/test");
        assert!(!inst.is_sandboxed());

        inst.yolo_mode = true;
        assert!(inst.is_yolo_mode());
        assert!(!inst.is_sandboxed());
    }

    #[test]
    #[serial_test::serial]
    fn test_yolo_envvar_command_is_quoted() {
        // EnvVar values containing JSON must be shell-escaped to prevent
        // the inner bash from expanding special characters ({, *, ").
        let result = format_env_var_prefix("OPENCODE_PERMISSION", r#"{"*":"allow"}"#, "opencode");
        assert_eq!(result, r#"OPENCODE_PERMISSION='{"*":"allow"}' opencode"#);
    }

    #[test]
    #[serial_test::serial]
    fn launch_shell_preserves_env_values_and_non_posix_login_state() {
        let temp = tempfile::tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(temp.path());
        let _path = crate::session::test_support::install_login_shell_path_command(
            temp.path(),
            "probe",
            "#!/bin/sh\nprintf '%s\\n' \"$OPENCODE_PERMISSION\" \"$AOE_TEST_LOGIN\"\n",
        );
        for file in [".profile", ".bash_profile"] {
            std::fs::write(temp.path().join(file), "export AOE_TEST_LOGIN=login\n").unwrap();
        }
        let value = r#"{"*":"allow","literal":"$HOME"}"#;
        let command = format_env_var_prefix(
            "OPENCODE_PERMISSION",
            value,
            &shell_escape(temp.path().join("bin/probe").to_str().unwrap()),
        );
        for (shell, login) in [
            ("/bin/sh", "login"),
            ("/usr/bin/fish", "parent"),
            ("/usr/bin/nu", "parent"),
        ] {
            let _shell = EnvGuard::set(&[("SHELL", shell)]);
            let wrapped =
                wrap_command_ignore_suspend(&command, temp.path().to_str().unwrap(), &[], &[]);
            let output = std::process::Command::new("/bin/sh")
                .args(["-c", &wrapped])
                .env("AOE_TEST_LOGIN", "parent")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                String::from_utf8(output.stdout).unwrap(),
                format!("{value}\n{login}\n")
            );
        }
    }
    #[test]
    #[serial_test::serial]
    fn test_wrap_command_reasserts_working_dir_after_login_shell() {
        let temp = tempfile::tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(temp.path());
        let _path = crate::session::test_support::install_login_shell_path_command(
            temp.path(),
            "claude",
            "#!/bin/sh\nexit 0\n",
        );
        let mut failures = Vec::new();
        for suffix in ["plain", "line\nand\rbreak"] {
            let cwd = temp.path().join(format!("cwd-{suffix}"));
            let store = temp.path().join(format!("store-{suffix}"));
            std::fs::create_dir_all(&cwd).unwrap();
            std::fs::create_dir_all(&store).unwrap();
            let _store = EnvGuard::set(&[("CLAUDE_CONFIG_DIR", &store)]);
            let mut instance = Instance::new("script", cwd.to_str().unwrap());
            instance.tool = "claude".into();
            instance.command = "claude".into();
            let execution = instance.resolve_native_execution(None).unwrap();
            let payload = "printf '%s\\0%s' \"$PWD\" \"$CLAUDE_CONFIG_DIR\"";
            for (context, wrapped) in [
                (
                    "host",
                    wrap_command_ignore_suspend(
                        payload,
                        cwd.to_str().unwrap(),
                        &execution.routing,
                        execution.case_insensitive_routing,
                    ),
                ),
                (
                    "container script",
                    wrap_native_container_command(payload, Some(&execution)).unwrap(),
                ),
            ] {
                let output = std::process::Command::new("/bin/sh")
                    .args(["-c", &wrapped])
                    .output()
                    .unwrap();
                let expected = format!("{}\0{}", cwd.display(), store.display()).into_bytes();
                if !output.status.success() || output.stdout != expected {
                    failures.push(format!(
                        "{context} {suffix:?}: status={} stdout={:?} stderr={:?}",
                        output.status,
                        output.stdout,
                        String::from_utf8_lossy(&output.stderr)
                    ));
                }
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    // Tests for get_tool_command
    #[test]
    fn test_get_tool_command_default_claude() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.tool = "claude".to_string();
        assert_eq!(inst.get_tool_command(), "claude");
    }

    #[test]
    fn test_get_tool_command_opencode() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.tool = "opencode".to_string();
        assert_eq!(inst.get_tool_command(), "opencode");
    }

    #[test]
    fn test_get_tool_command_codex() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.tool = "codex".to_string();
        assert_eq!(inst.get_tool_command(), "codex");
    }

    #[test]
    fn test_get_tool_command_gemini() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.tool = "gemini".to_string();
        assert_eq!(inst.get_tool_command(), "gemini");
    }

    #[test]
    fn test_get_tool_command_unknown_tool() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.tool = "unknown".to_string();
        assert_eq!(inst.get_tool_command(), "bash");
    }

    #[test]
    fn test_get_tool_command_custom_command() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.tool = "claude".to_string();
        inst.command = "claude --resume abc123".to_string();
        assert_eq!(inst.get_tool_command(), "claude --resume abc123");
    }

    #[test]
    fn test_build_claude_resume_flags_existing() {
        let session_id = "abc123-def456";
        let flags = build_resume_flags("claude", session_id, true);
        assert_eq!(flags, "--resume abc123-def456");
    }

    #[test]
    fn test_build_claude_session_id_flags_new() {
        let session_id = "abc123-def456";
        let flags = build_resume_flags("claude", session_id, false);
        assert_eq!(flags, "--session-id abc123-def456");
    }

    #[test]
    fn test_build_opencode_resume_flags() {
        let session_id = "session-789";
        let flags = build_resume_flags("opencode", session_id, false);
        assert_eq!(flags, "--session session-789");

        let flags = build_resume_flags("opencode", session_id, true);
        assert_eq!(flags, "--session session-789");
    }

    #[test]
    fn test_build_resume_flags_for_resume_only_agents() {
        let session_id = "session-789";
        assert_eq!(
            build_resume_flags("vibe", session_id, true),
            "--resume session-789"
        );
        assert_eq!(
            build_resume_flags("copilot", session_id, true),
            "--session-id session-789"
        );
    }

    #[test]
    fn test_build_resume_flags_rejects_invalid_id() {
        let flags = build_resume_flags("claude", "$(rm -rf /)", true);
        assert_eq!(flags, "");

        let flags = build_resume_flags("opencode", "id; echo pwned", false);
        assert_eq!(flags, "");
    }

    #[test]
    fn fork_flags_reject_invalid_ids() {
        assert_eq!(
            build_fork_flags("claude", "$(rm -rf /)", "child"),
            String::new()
        );
        assert_eq!(
            build_fork_flags("claude", "parent", "; echo pwned"),
            String::new()
        );
    }

    #[test]
    fn fork_flags_empty_for_unsupported_agent() {
        assert_eq!(build_fork_flags("cursor", "parent", "child"), String::new());
    }

    #[test]
    fn fork_flags_for_codex_and_opencode() {
        // Codex: `fork <parent>` subcommand. child_id unused (codex mints its own).
        let codex = build_fork_flags("codex", "parent-id", "ignored-child");
        assert_eq!(codex, "fork parent-id");
        // OpenCode: resume the parent session and add --fork. agent mints new id.
        let oc = build_fork_flags("opencode", "parent-id", "ignored-child");
        assert_eq!(oc, "--session parent-id --fork");
    }

    #[test]
    fn fork_command_inserts_codex_subcommand_after_binary() {
        // codex fork must sit right after the binary, before other flags,
        // mirroring how codex `resume` is inserted as a subcommand.
        let mut inst = Instance::new("Forked", "/tmp/x");
        inst.tool = "codex".to_string();
        inst.agent_session_id = Some("child-ignored-by-codex".to_string());
        inst.resume_intent = ResumeIntent::Fork {
            from: "parent-1234".to_string(),
        };
        let mut cmd = "codex --some-flag".to_string();
        inst.apply_session_flags(&mut cmd, "test", inst.resolved_agent(), None)
            .unwrap();
        assert_eq!(cmd, "codex fork parent-1234 --some-flag");
    }

    #[test]
    #[serial_test::serial]
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
    fn fork_command_appends_opencode_flags() {
        let mut inst = Instance::new("Forked", "/tmp/x");
        inst.tool = "opencode".to_string();
        inst.agent_session_id = Some("child-ignored".to_string());
        inst.resume_intent = ResumeIntent::Fork {
            from: "parent-9999".to_string(),
        };
        let mut cmd = "opencode".to_string();
        inst.apply_session_flags(&mut cmd, "test", inst.resolved_agent(), None)
            .unwrap();
        assert_eq!(cmd, "opencode --session parent-9999 --fork");
    }

    #[test]
    fn test_build_unknown_tool_resume_flags() {
        let flags = build_resume_flags("mistral", "session-123", false);
        assert!(flags.is_empty());
    }

    #[test]
    fn environment_defines_path_only_for_the_assigning_form() {
        // A pass-through entry hands the pane AoE's own PATH, so the probed
        // binary is the one that runs; an assignment can front a different pi.
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
    fn test_build_pi_resume_flags() {
        // An id already on file resumes with `--session`, which every pi
        // version takes. A fresh launch pins the id AoE minted with
        // `--session-id`, which creates the session when it is missing.
        let flags = build_resume_flags("pi", "019342ab-1234-7def-8901-abcdef012345", true);
        assert_eq!(flags, "--session 019342ab-1234-7def-8901-abcdef012345");

        let flags_new = build_resume_flags("pi", "019342ab-1234-7def-8901-abcdef012345", false);
        assert_eq!(
            flags_new,
            "--session-id 019342ab-1234-7def-8901-abcdef012345"
        );
    }

    #[test]
    fn test_has_custom_command_empty() {
        let inst = Instance::new("test", "/tmp/test");
        assert!(!inst.has_custom_command());
    }

    #[test]
    fn test_has_custom_command_same_as_agent_binary() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.tool = "claude".to_string();
        inst.command = "claude".to_string();
        assert!(!inst.has_custom_command());
    }

    #[test]
    fn test_has_custom_command_override() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.tool = "claude".to_string();
        inst.command = "my-wrapper".to_string();
        assert!(inst.has_custom_command());
    }

    #[test]
    fn test_has_custom_command_unknown_tool() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.tool = "unknown_agent".to_string();
        inst.command = "unknown_agent".to_string();
        assert!(inst.has_custom_command());
    }

    #[test]
    fn test_has_command_override_extra_args_only() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.tool = "claude".to_string();
        inst.extra_args = "--model opus".to_string();
        assert!(!inst.has_command_override());
        assert!(inst.has_custom_command());
    }

    #[test]
    fn test_expects_shell() {
        let mut inst = Instance::new("test", "/tmp/test");
        assert!(!inst.expects_shell());

        inst.tool = "unknown-tool".to_string();
        inst.command = String::new();
        assert!(inst.expects_shell());

        inst.tool = "claude".to_string();
        inst.command = "bash".to_string();
        assert!(inst.expects_shell());

        inst.command = "my-agent".to_string();
        assert!(!inst.expects_shell());
    }

    #[test]
    fn test_build_host_command_basic() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.tool = "codex".to_string();
        let (cmd, _, _) = inst
            .build_host_command(crate::agents::get_agent("codex"), None)
            .unwrap();
        assert!(cmd.is_some());
        assert!(cmd.as_ref().unwrap().contains("codex"));
    }

    #[test]
    fn test_build_host_command_with_yolo() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.tool = "codex".to_string();
        inst.yolo_mode = true;
        let (cmd, _, _) = inst
            .build_host_command(crate::agents::get_agent("codex"), None)
            .unwrap();
        let cmd_str = cmd.unwrap();
        let agent = crate::agents::get_agent("codex").unwrap();
        match agent.yolo.as_ref().unwrap() {
            crate::agents::YoloMode::CliFlag(flag) => assert!(cmd_str.contains(flag)),
            crate::agents::YoloMode::EnvVar(key, _) => assert!(cmd_str.contains(key)),
            crate::agents::YoloMode::AlwaysYolo => {}
        }
    }

    #[test]
    fn test_build_host_command_with_resume() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.tool = "claude".to_string();
        inst.agent_session_id = Some("ses_abc123def456".to_string());
        let (cmd, _, _) = inst
            .build_host_command(crate::agents::get_agent("claude"), None)
            .unwrap();
        let cmd_str = cmd.unwrap();
        assert!(cmd_str.contains("ses_abc123def456"));
        assert!(cmd_str.contains("--session-id") || cmd_str.contains("--resume"));
    }

    #[test]
    fn test_build_host_command_antigravity_forces_color() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.tool = "antigravity".to_string();
        let (cmd, _, _) = inst
            .build_host_command(crate::agents::get_agent("antigravity"), None)
            .unwrap();
        let cmd_str = cmd.unwrap();

        assert!(cmd_str.contains("env -u NO_COLOR"));
        assert!(cmd_str.contains("TERM=xterm-256color"));
        assert!(cmd_str.contains("COLORTERM=truecolor"));
        assert!(cmd_str.contains("agy"));
    }

    #[test]
    fn test_build_host_command_kiro_uses_chat_subcommand() {
        // Regression: Kiro must launch via `kiro-cli chat` so the binary
        // accepts chat-scoped flags. Bare `kiro-cli` rejects --trust-all-tools.
        let mut inst = Instance::new("test", "/tmp/test");
        inst.tool = "kiro".to_string();
        let (cmd, _, _) = inst
            .build_host_command(crate::agents::get_agent("kiro"), None)
            .unwrap();
        assert!(cmd.unwrap().contains("kiro-cli chat"));
    }

    #[test]
    fn test_build_host_command_kiro_yolo_after_chat() {
        // YOLO flag must follow the `chat` subcommand, not precede it.
        let mut inst = Instance::new("test", "/tmp/test");
        inst.tool = "kiro".to_string();
        inst.yolo_mode = true;
        let (cmd, _, _) = inst
            .build_host_command(crate::agents::get_agent("kiro"), None)
            .unwrap();
        let cmd_str = cmd.unwrap();
        let chat_pos = cmd_str
            .find("kiro-cli chat")
            .expect("chat subcommand present");
        let yolo_pos = cmd_str
            .find("--trust-all-tools")
            .expect("yolo flag present");
        assert!(
            yolo_pos > chat_pos,
            "--trust-all-tools must come after `kiro-cli chat` \
             (chat at {chat_pos}, flag at {yolo_pos})"
        );
    }

    #[test]
    fn test_build_host_command_custom_override_skips_subcommand() {
        // A user command override is passed through verbatim; AoE must not
        // inject a launch subcommand into it (the user is in full control).
        let mut inst = Instance::new("test", "/tmp/test");
        inst.tool = "kiro".to_string();
        inst.command = "kiro-cli chat --trust-all-tools".to_string();
        let (cmd, _, _) = inst
            .build_host_command(crate::agents::get_agent("kiro"), None)
            .unwrap();
        let cmd_str = cmd.unwrap();
        // Exactly one "chat" token (no doubled `chat chat`).
        assert_eq!(
            cmd_str.matches("chat").count(),
            1,
            "no duplicate subcommand"
        );
    }

    #[test]
    fn test_selected_agent_args_combines_command_and_extra() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.tool = "kiro".to_string();
        inst.extra_args = "--agent custom-agent".to_string();
        assert_eq!(
            crate::agents::parse_selected_agent(&inst.selected_agent_args(), "--agent"),
            Some("custom-agent".to_string())
        );

        // Agent named inside a command override is also found.
        let mut inst2 = Instance::new("test", "/tmp/test");
        inst2.tool = "kiro".to_string();
        inst2.command = "kiro-cli chat --agent custom-agent".to_string();
        assert_eq!(
            crate::agents::parse_selected_agent(&inst2.selected_agent_args(), "--agent"),
            Some("custom-agent".to_string())
        );

        // extra_args is appended after the command override, so a per-session
        // --agent there wins over one baked into the override (last wins).
        let mut inst3 = Instance::new("test", "/tmp/test");
        inst3.tool = "kiro".to_string();
        inst3.command = "kiro-cli chat --agent from-command".to_string();
        inst3.extra_args = "--agent from-extra".to_string();
        assert_eq!(
            crate::agents::parse_selected_agent(&inst3.selected_agent_args(), "--agent"),
            Some("from-extra".to_string())
        );
    }

    #[test]
    fn test_build_host_custom_command_antigravity_forces_color() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.tool = "antigravity".to_string();
        inst.command = "agy --some-flag".to_string();
        let (cmd, _, _) = inst
            .build_host_command(crate::agents::get_agent("antigravity"), None)
            .unwrap();
        let cmd_str = cmd.unwrap();

        assert!(cmd_str.contains("env -u NO_COLOR"));
        assert!(cmd_str.contains("TERM=xterm-256color"));
        assert!(cmd_str.contains("COLORTERM=truecolor"));
        assert!(cmd_str.contains("agy --some-flag"));
    }

    #[test]
    fn test_build_host_command_codex_forces_color() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.tool = "codex".to_string();
        let (cmd, _, _) = inst
            .build_host_command(crate::agents::get_agent("codex"), None)
            .unwrap();
        let cmd_str = cmd.unwrap();

        assert!(cmd_str.contains("env -u NO_COLOR"));
        assert!(cmd_str.contains("TERM=xterm-256color"));
        assert!(cmd_str.contains("COLORTERM=truecolor"));
        assert!(cmd_str.contains("codex"));
    }

    #[test]
    fn test_build_host_command_color_env_is_limited_to_color_sensitive_agents() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.tool = "cursor".to_string();
        let (cmd, _, _) = inst
            .build_host_command(crate::agents::get_agent("cursor"), None)
            .unwrap();
        let cmd_str = cmd.unwrap();

        assert!(!cmd_str.contains("env -u NO_COLOR"));
        assert!(!cmd_str.contains("TERM=xterm-256color"));
        assert!(!cmd_str.contains("COLORTERM=truecolor"));
    }
}
