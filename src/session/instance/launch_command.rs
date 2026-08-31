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
///
/// Rewritten when the content differs so an upgrade ships its own version.
pub(super) fn pi_extension_path() -> Result<PathBuf> {
    const SOURCE: &str = crate::session::instance::PI_SESSION_EXTENSION;
    let dir = crate::session::get_app_dir()?.join("agent-extensions");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("pi-aoe-session-id.js");
    if std::fs::read_to_string(&path).ok().as_deref() != Some(SOURCE) {
        crate::session::replace_file_no_follow(&path, SOURCE.as_bytes())?;
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
    match &agent.resume_strategy {
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
        ResumeStrategy::Unsupported => String::new(),
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
            match agent.resume_strategy {
                ResumeStrategy::Flag(resume_flag) => {
                    format!("{resume_flag} {parent_id} {fork_flag}")
                }
                _ => String::new(),
            }
        }
        ForkStrategy::Unsupported => String::new(),
    }
}

/// Splice `part` into `cmd`: insert it right after the binary (before other
/// flags) when it is a subcommand, else append it. Shared by the resume and
/// fork launch-flag builders.
pub(super) fn splice_subcommand_or_append(cmd: &mut String, part: &str, is_subcommand: bool) {
    if is_subcommand {
        if let Some(space_pos) = cmd.find(' ') {
            let binary = &cmd[..space_pos];
            let flags = &cmd[space_pos..];
            *cmd = format!("{} {}{}", binary, part, flags);
        } else {
            *cmd = format!("{} {}", cmd, part);
        }
    } else {
        *cmd = format!("{} {}", cmd, part);
    }
}

pub(super) fn append_resume_flags(
    tool: &str,
    session_id: Option<&str>,
    is_existing_session: bool,
    cmd: &mut String,
    context: &str,
) -> bool {
    use crate::agents::{get_agent, ResumeStrategy};

    if let Some(session_id) = session_id {
        let resume_part = build_resume_flags(tool, session_id, is_existing_session);
        if resume_part.is_empty() {
            return false;
        }
        let is_subcommand = matches!(
            get_agent(tool).map(|a| &a.resume_strategy),
            Some(ResumeStrategy::Subcommand(_))
        );
        splice_subcommand_or_append(cmd, &resume_part, is_subcommand);
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
/// `working_dir` is re-asserted with `cd` as the first statement in that
/// script, after the login shell's profile/rc files have run. tmux's
/// `new-session -c` only sets the shell's initial cwd, and a `-l` login
/// shell's rc files (or an nvm/direnv hook) can `cd` away before the agent
/// starts; re-cd-ing here wins regardless (#3265).
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

    /// Construct the command only after hook execution has completed. Keeping
    /// this phase hook-free prevents a revalidation retry from replaying user
    /// code while the lifecycle lock is held.
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

            // Snapshot only after container hooks have had their final chance
            // to mutate OMP dotenv/config routing, but before any executable
            // pane command exists.
            let omp_capture_plan = self
                .omp_capture_options()
                .and_then(|options| self.resolve_omp_capture_plan(&options));

            let launch_cmd = self.get_launch_command();
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

            // Pi publishes its conversation from inside the container through
            // the same extension, reaching the instance dir and the extension
            // file by bind-mount (see `container_config`).
            let pi_extension = self.pi_extension_launch();
            if let Some((ref flag, _)) = pi_extension {
                // Empty for a container: the extension is discovered there
                // rather than named on the command line.
                tool_cmd.push_str(flag);
                self.pi_extension_launched = true;
            }
            let is_existing = self.apply_session_flags(&mut tool_cmd, "sandboxed");
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
            if let Some((_, ref env)) = pi_extension {
                // `KEY=VALUE ` from the host form, passed as a docker `-e`.
                env_info
                    .docker_args
                    .push_str(&format!(" -e {}", shell_escape(env.trim())));
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
                // Pin every routing input, including explicit empty values and
                // true absence, so tmux's frozen server environment cannot
                // select another OMP store. The in-pane fingerprint still
                // detects login-file drift.
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
        // Resolve after `on_launch`. The snapshot is checked inside the
        // profile environment assignment scope executed by the login shell;
        // startup-file routing drift therefore disables capture.
        let omp_capture_plan = self
            .omp_capture_options()
            .and_then(|options| self.resolve_omp_capture_plan(&options));

        let profile = self.effective_profile();
        let mut env_prefix = status_hook_env_prefix(&profile, &self.id, agent);
        // Pi publishes its own conversation through an AoE extension; the flag
        // rides the built-in command only, an override being unvouched.
        let pi_extension = self.pi_extension_launch();
        if let Some((_, ref env)) = pi_extension {
            env_prefix.push_str(env);
            self.pi_extension_launched = true;
        }
        let env_prefix = env_prefix;

        if self.command.is_empty() {
            match crate::agents::get_agent(&self.tool) {
                Some(a) => {
                    let mut cmd = a.launch_base_command();
                    if let Some((ref flag, _)) = pi_extension {
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
                    let is_existing = self.apply_session_flags(&mut cmd, "host agent");
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
            if !self.extra_args.is_empty() {
                cmd = format!("{} {}", cmd, self.extra_args);
            }
            if self.is_yolo_mode() {
                if let Some(yolo) = agent.and_then(|a| a.yolo.as_ref()) {
                    apply_yolo_mode(&mut cmd, yolo, false);
                }
            }
            let is_existing = self.apply_session_flags(&mut cmd, "host custom");
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

    // The sidecar env var has to survive into the docker argv, not just be
    // computed: nothing in CI runs a container to catch it going missing.
    #[test]
    #[serial_test::serial]
    fn sandboxed_pi_launch_line_carries_the_sidecar_env() {
        let (_guard, _base, _tmp) = crate::hooks::test_support::BaseGuard::ready();
        let temp_home = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::EnvGuard::set(&[("HOME", temp_home.path())]);

        let project = temp_home.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let mut inst = Instance::new("pi-argv", project.to_str().unwrap());
        inst.tool = "pi".to_string();
        inst.sandbox_info = Some(crate::session::SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test:latest".to_string(),
            container_name: "aoe-pi-argv".to_string(),
            extra_env: None,
            custom_instruction: None,
            container_workdir: Some("/workspace".to_string()),
            before_start_env: Vec::new(),
        });

        let (cmd, _, _, _) = inst
            .build_launch_command()
            .expect("a sandboxed launch line");
        let cmd = cmd.expect("a command");
        assert!(
            cmd.contains(&format!(
                "AOE_PI_SESSION_ID_FILE={}/{}/session_id",
                crate::session::container_config::PI_SIDECAR_DIR_IN_CONTAINER,
                inst.id
            )),
            "the pane cannot publish without this: {cmd}"
        );
        assert!(
            !cmd.contains(" -e /") || !cmd.contains("aoe-session-id.js"),
            "no `-e` path may reach a container launch: {cmd}"
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

        let (flag, env) = inst.pi_extension_launch().expect("sandboxed pi publishes");
        assert!(flag.is_empty(), "no `-e` may reach a container launch");
        assert_eq!(
            env.trim(),
            format!(
                "AOE_PI_SESSION_ID_FILE={}/{}/session_id",
                crate::session::container_config::PI_SIDECAR_DIR_IN_CONTAINER,
                inst.id
            )
        );
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
    fn test_yolo_envvar_survives_suspend_wrapper() {
        let cmd = format_env_var_prefix("OPENCODE_PERMISSION", r#"{"*":"allow"}"#, "opencode");
        let wrapped = wrap_command_ignore_suspend(&cmd, "/tmp/proj");
        assert!(
            wrapped.contains(r#"OPENCODE_PERMISSION='{"*":"allow"}' opencode"#),
            "wrapped command should preserve the env assignment: {wrapped}",
        );
    }

    #[test]
    #[serial_test::serial(shell_env)]
    fn test_wrap_command_uses_stdin_script() {
        for shell in &["/bin/bash", "/bin/zsh", "/usr/bin/fish", "/usr/bin/nu"] {
            let _shell = EnvGuard::set(&[("SHELL", shell)]);
            let wrapped = wrap_command_ignore_suspend("claude", "/tmp/proj");
            assert!(
                wrapped.contains("/dev/fd/3 3<<'AOE_LAUNCH_BODY'"),
                "{shell}: {wrapped}"
            );
            assert!(wrapped.contains("\nstty susp undef\nexec env claude\n"));
            assert!(!wrapped.contains(" -c "));
        }
    }

    #[test]
    #[serial_test::serial(shell_env)]
    fn test_wrap_command_posix_shell_uses_login() {
        let _shell = EnvGuard::set(&[("SHELL", "/bin/zsh")]);
        let wrapped = wrap_command_ignore_suspend("claude", "/tmp/proj");
        assert!(
            wrapped.starts_with("'/bin/zsh' -l /dev/fd/3 "),
            "POSIX shell should use a login descriptor script: {wrapped}",
        );
    }

    #[test]
    #[serial_test::serial(shell_env)]
    fn test_wrap_command_fish_skips_login() {
        let _shell = EnvGuard::set(&[("SHELL", "/usr/bin/fish")]);
        let wrapped = wrap_command_ignore_suspend("claude", "/tmp/proj");
        // The bash fallback must not load bash login files because the user's
        // PATH setup belongs to fish.
        assert!(
            wrapped.starts_with("'bash' /dev/fd/3 "),
            "fish should use a non-login bash descriptor script: {wrapped}",
        );
    }

    #[test]
    #[serial_test::serial(shell_env)]
    fn test_wrap_command_nu_skips_login() {
        let _shell = EnvGuard::set(&[("SHELL", "/usr/bin/nu")]);
        let wrapped = wrap_command_ignore_suspend("claude", "/tmp/proj");
        assert!(
            wrapped.starts_with("'bash' /dev/fd/3 "),
            "nu should use a non-login bash descriptor script: {wrapped}",
        );
    }

    /// #3265: a login shell's own profile/rc files can `cd` elsewhere
    /// (a stray line in `~/.bashrc`, or a legitimate nvm/pyenv/direnv hook)
    /// after tmux's `-c` has already set the pane's cwd, silently landing
    /// the agent in the wrong directory. The wrapper must re-assert
    /// `working_dir` inside the login shell's own script, after profile
    /// sourcing, so it wins regardless of what those files did.
    ///
    /// `#[serial]` on the default key, not `shell_env`: this resolves `bash`
    /// through the inherited `PATH`, and every test that mutates `PATH`
    /// process-globally (`update::install`, `acp::node`, `acp::acp_client`)
    /// carries the default key, so `shell_env` bought no exclusion against
    /// them. Since #3421 a scrub racing the `which` is a silent skip rather
    /// than a failure. The `shell_env` holder this stops excluding touches
    /// only `TERM`/`COLORTERM`/`FORCE_COLOR`/`NO_COLOR`, and `Command`
    /// snapshots the environment at spawn, so the exposure is that instant.
    #[test]
    #[serial_test::serial]
    fn test_wrap_command_reasserts_working_dir_after_login_shell() {
        // The wrapper execs `$SHELL`, so it has to be a shell that exists here.
        let Ok(bash) = which::which("bash") else {
            eprintln!("skipping: bash not found on PATH");
            return;
        };
        // The guard restores on unwind; the resolved path matters separately,
        // because `wrap_command_ignore_suspend` execs `$SHELL` below. The
        // `repo_config` hook tests used to read this override too and now pin
        // their own (#3449).
        let _shell = EnvGuard::set(&[("SHELL", &bash)]);
        let temp = tempfile::tempdir().unwrap();
        let working_dir = temp.path().join("some project's dir");
        std::fs::create_dir(&working_dir).unwrap();
        let wrapped = wrap_command_ignore_suspend("pwd", working_dir.to_str().unwrap());
        // The cd is the first statement inside the login shell's stdin script,
        // after profile sourcing, before disabling suspend and exec'ing.
        assert!(
            wrapped.contains("3<<'AOE_LAUNCH_BODY'\ncd "),
            "the cd must open the login shell's stdin script: {wrapped}",
        );
        assert!(
            wrapped.contains("|| exit 1\nstty susp undef"),
            "the cd must exit-on-failure before disabling suspend: {wrapped}",
        );
        let output = std::process::Command::new(&bash)
            .args(["-c", &wrapped])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "wrapped command failed: {}",
            String::from_utf8_lossy(&output.stderr),
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            working_dir.to_string_lossy(),
        );
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
        inst.apply_session_flags(&mut cmd, "test");
        assert_eq!(cmd, "codex fork parent-1234 --some-flag");
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
        inst.apply_session_flags(&mut cmd, "test");
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
            .build_host_command(crate::agents::get_agent("codex"))
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
            .build_host_command(crate::agents::get_agent("codex"))
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
            .build_host_command(crate::agents::get_agent("claude"))
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
            .build_host_command(crate::agents::get_agent("antigravity"))
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
            .build_host_command(crate::agents::get_agent("kiro"))
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
            .build_host_command(crate::agents::get_agent("kiro"))
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
            .build_host_command(crate::agents::get_agent("kiro"))
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
            .build_host_command(crate::agents::get_agent("antigravity"))
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
            .build_host_command(crate::agents::get_agent("codex"))
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
            .build_host_command(crate::agents::get_agent("cursor"))
            .unwrap();
        let cmd_str = cmd.unwrap();

        assert!(!cmd_str.contains("env -u NO_COLOR"));
        assert!(!cmd_str.contains("TERM=xterm-256color"));
        assert!(!cmd_str.contains("COLORTERM=truecolor"));
    }
}
