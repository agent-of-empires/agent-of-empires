//! Lifecycle and host hook configuration, resolution, display, and execution.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use crate::session::config::profile_config;

#[derive(Debug, Clone)]
pub enum HookProgress {
    Started(String),
    Output(String),
}

/// Produced only by `run_hook_with_timeout`, which runs only under a
/// `HookTimeoutScope`. Recovery downcasts it from `anyhow::Error`, so wrap it
/// with `.context`, never re-stringify it with `anyhow!`.
#[derive(Debug, Clone, thiserror::Error)]
#[error("hook timed out after {timeout_secs}s: {cmd}")]
pub struct HookTimeout {
    pub cmd: String,
    pub timeout_secs: u64,
}

/// Lifecycle hook commands; each accepts a string or an array in TOML.
/// `on_create` failures abort creation; `on_launch` and `on_destroy` failures
/// only warn, and `on_destroy` runs before worktree/sandbox cleanup.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HooksConfig {
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "crate::session::serde_helpers::string_or_vec"
    )]
    pub on_create: Vec<String>,
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "crate::session::serde_helpers::string_or_vec"
    )]
    pub on_launch: Vec<String>,
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "crate::session::serde_helpers::string_or_vec"
    )]
    pub on_destroy: Vec<String>,
}

impl HooksConfig {
    pub fn is_empty(&self) -> bool {
        self.on_create.is_empty() && self.on_launch.is_empty() && self.on_destroy.is_empty()
    }
}

/// Host-side hooks, profile/global only (never from a repo). Each prints
/// `KEY=VALUE` lines that are injected into the launch; stdout is never logged,
/// and a non-zero exit aborts the launch.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HostHooksConfig {
    /// Runs before a sandbox container comes up; pairs reach the container as
    /// inherited env, applied first (the container list is first-wins).
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "crate::session::serde_helpers::string_or_vec"
    )]
    pub before_start: Vec<String>,
    /// Runs before every host session launch; pairs are applied after the
    /// static `environment`, so a minted value wins.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "crate::session::serde_helpers::string_or_vec"
    )]
    pub before_session: Vec<String>,
}

impl HostHooksConfig {
    pub fn is_empty(&self) -> bool {
        self.before_start.is_empty() && self.before_session.is_empty()
    }
}

fn has_create_or_launch(hooks: HooksConfig) -> Option<HooksConfig> {
    (!hooks.on_create.is_empty() || !hooks.on_launch.is_empty()).then_some(hooks)
}

/// Global+profile hooks, or `None` without any `on_create`/`on_launch`.
pub fn resolve_global_profile_hooks(profile: &str) -> Option<HooksConfig> {
    has_create_or_launch(profile_config::resolve_config_or_warn(profile).hooks)
}

/// Trusted repo hooks replace global ones per type (not append).
fn merge_hooks_with_config(profile: &str, repo_hooks: HooksConfig) -> Option<HooksConfig> {
    let mut base = profile_config::resolve_config_or_warn(profile).hooks;
    if !repo_hooks.on_create.is_empty() {
        base.on_create = repo_hooks.on_create;
    }
    if !repo_hooks.on_launch.is_empty() {
        base.on_launch = repo_hooks.on_launch;
    }
    has_create_or_launch(base)
}

/// Like the execution merge but keeps `on_destroy` and never collapses, for display.
fn apply_repo_hook_overrides(mut base: HooksConfig, repo_hooks: &HooksConfig) -> HooksConfig {
    for (base_cmds, repo_cmds) in [
        (&mut base.on_create, &repo_hooks.on_create),
        (&mut base.on_launch, &repo_hooks.on_launch),
        (&mut base.on_destroy, &repo_hooks.on_destroy),
    ] {
        if !repo_cmds.is_empty() {
            *base_cmds = repo_cmds.clone();
        }
    }
    base
}

/// Every command that will run, for the trust dialog.
pub fn merge_hooks_for_display(profile: &str, repo_hooks: &HooksConfig) -> HooksConfig {
    apply_repo_hook_overrides(
        profile_config::resolve_config_or_warn(profile).hooks,
        repo_hooks,
    )
}

pub struct HookDisplayGroup {
    pub name: &'static str,
    /// The repo defined this type, overriding global.
    pub from_repo: bool,
    pub commands: Vec<String>,
}

impl HookDisplayGroup {
    pub fn source_label(&self) -> &'static str {
        if self.from_repo {
            " (from repo)"
        } else {
            " (from global config)"
        }
    }
}

/// Non-empty hook types labeled by source; `on_destroy` only when `include_destroy`.
pub fn hook_display_groups(
    merged: &HooksConfig,
    repo: &HooksConfig,
    include_destroy: bool,
) -> Vec<HookDisplayGroup> {
    let mut types: Vec<(&'static str, &[String], &[String])> = vec![
        ("on_create", &merged.on_create, &repo.on_create),
        ("on_launch", &merged.on_launch, &repo.on_launch),
    ];
    if include_destroy {
        types.push(("on_destroy", &merged.on_destroy, &repo.on_destroy));
    }
    types
        .into_iter()
        .filter(|(_, merged_cmds, _)| !merged_cmds.is_empty())
        .map(|(name, merged_cmds, repo_cmds)| HookDisplayGroup {
            name,
            from_repo: !repo_cmds.is_empty(),
            commands: merged_cmds.to_vec(),
        })
        .collect()
}

/// A merged hook set plus the repo it drew on, so a failure can name the file
/// that declared the failing commands.
#[derive(Debug, Clone)]
pub struct ResolvedHooks {
    hooks: HooksConfig,
    profile: String,
    /// Set only when trusted repo hooks were merged in.
    repo_root: Option<PathBuf>,
}

impl ResolvedHooks {
    pub fn hooks(&self) -> &HooksConfig {
        &self.hooks
    }

    /// Global and profile hooks only; `None` without `on_create`/`on_launch`.
    pub fn global(profile: &str) -> Option<Self> {
        resolve_global_profile_hooks(profile).map(|hooks| Self {
            hooks,
            profile: profile.to_string(),
            repo_root: None,
        })
    }

    /// Trusted hooks read from `repo_root` (a [`RepoTrust::project_path`](super::RepoTrust))
    /// over global and profile.
    pub fn with_repo(profile: &str, repo_root: &Path, repo_hooks: HooksConfig) -> Option<Self> {
        merge_hooks_with_config(profile, repo_hooks).map(|hooks| Self {
            hooks,
            profile: profile.to_string(),
            repo_root: Some(repo_root.to_path_buf()),
        })
    }

    /// Names the config file that declared this set's `hook_type` commands.
    /// Each layer's own declaration is matched against the commands, most
    /// specific first; `None` when none matches.
    pub fn origin_hint(&self, hook_type: &str) -> Option<String> {
        let profile = self.profile.as_str();
        let of_type = |h: HooksConfig| match hook_type {
            "on_create" => Some(h.on_create),
            "on_launch" => Some(h.on_launch),
            "on_destroy" => Some(h.on_destroy),
            _ => None,
        };
        let commands = of_type(self.hooks.clone()).filter(|c| !c.is_empty())?;
        let file_hooks = |path: PathBuf| -> Option<(HooksConfig, PathBuf)> {
            let content = std::fs::read_to_string(&path).ok()?;
            let hooks = toml::from_str::<super::RepoConfig>(&content)
                .ok()?
                .hooks()?;
            Some((hooks, path))
        };

        let repo = self
            .repo_root
            .as_deref()
            .and_then(super::resolved_repo_config_path)
            .and_then(file_hooks);
        // Profile overrides are sparse: no `hooks` section means global supplied them.
        let profile_layer = profile_config::load_profile_config(profile)
            .ok()
            .and_then(|pc| pc.overrides.get("hooks").cloned())
            .and_then(|v| serde_json::from_value::<HooksConfig>(v).ok())
            .zip(
                crate::session::get_profile_dir_path(profile)
                    .ok()
                    .map(|dir| dir.join("config.toml")),
            );
        let global = crate::session::Config::load().ok().map(|c| c.hooks).zip(
            crate::session::get_app_dir_path()
                .ok()
                .map(|dir| dir.join("config.toml")),
        );

        [repo, profile_layer, global]
            .into_iter()
            .flatten()
            .find(|(hooks, _)| of_type(hooks.clone()).as_ref() == Some(&commands))
            .map(|(_, path)| format!("declared in {} ([hooks] {hook_type})", path.display()))
    }
}

enum HookTarget<'a> {
    Local {
        project_path: &'a Path,
    },
    Container {
        container_name: &'a str,
        workdir: &'a str,
    },
}

impl HookTarget<'_> {
    fn in_container(&self) -> bool {
        matches!(self, HookTarget::Container { .. })
    }
}

#[derive(Clone, Copy, Default)]
struct HookSpawnOpts {
    /// Append `2>&1` so the streamed path reads one fd.
    merge_stderr: bool,
    /// Keep credential prompts off a TUI/web terminal; CLI leaves it attached.
    detach_tty: bool,
}

const PROMPT_SUPPRESS_ENV: &[(&str, &str)] = &[
    ("GIT_TERMINAL_PROMPT", "0"),
    ("GIT_ASKPASS", "true"),
    ("SSH_ASKPASS", "true"),
];

/// Local hooks run in `$SHELL`, container hooks in `bash` via `exec`, where env
/// must be passed as `-e` because host env does not propagate.
fn build_hook_command(
    cmd: &str,
    target: &HookTarget,
    opts: HookSpawnOpts,
    extra_env: &[(&'static str, String)],
) -> std::process::Command {
    let shell_cmd = if opts.merge_stderr {
        format!("{} 2>&1", cmd)
    } else {
        cmd.to_string()
    };
    let prompt_env = PROMPT_SUPPRESS_ENV.iter().filter(|_| opts.detach_tty);

    let mut command = match target {
        HookTarget::Local { project_path } => {
            let mut command = std::process::Command::new(crate::session::environment::user_shell());
            command.arg("-c").arg(shell_cmd).current_dir(project_path);
            command.envs(extra_env.iter().map(|(k, v)| (k, v)));
            command.envs(prompt_env.map(|(k, v)| (k, v)));
            #[cfg(unix)]
            if opts.detach_tty {
                use std::os::unix::process::CommandExt;
                // SAFETY: setsid is async-signal-safe, the only pre_exec requirement.
                unsafe {
                    command.pre_exec(|| {
                        nix::unistd::setsid().map_err(std::io::Error::other)?;
                        Ok(())
                    });
                }
            }
            command
        }
        HookTarget::Container {
            container_name,
            workdir,
        } => {
            let mut command = std::process::Command::new(crate::containers::runtime_binary());
            command.arg("exec").arg("--workdir").arg(workdir);
            let pairs = extra_env
                .iter()
                .map(|(k, v)| format!("{}={}", k, v))
                .chain(prompt_env.map(|(k, v)| format!("{}={}", k, v)));
            for pair in pairs {
                command.arg("-e").arg(pair);
            }
            command
                .arg(container_name)
                .arg("bash")
                .arg("-c")
                .arg(&shell_cmd);
            command
        }
    };
    if opts.detach_tty {
        command.stdin(std::process::Stdio::null());
    }
    command
}

/// Session env for lifecycle hooks; the key set is documented in
/// docs/guides/repo-config.md. `AOE_SESSION_BRANCH` needs a worktree and
/// `AOE_REPO_SLUG` a parseable `origin` remote.
pub(crate) fn lifecycle_env_vars(
    instance: &crate::session::Instance,
) -> Vec<(&'static str, String)> {
    let mut env = vec![
        ("AOE_SESSION_ID", instance.id.clone()),
        ("AOE_SESSION_TITLE", instance.title.clone()),
        ("AOE_PROJECT_PATH", instance.project_path.clone()),
        ("AOE_PROFILE", instance.effective_profile()),
        ("AOE_TOOL", instance.tool.clone()),
        ("AOE_GROUP_PATH", instance.group_path.clone()),
    ];
    if let Some(wt) = instance.worktree_info.as_ref() {
        env.push(("AOE_SESSION_BRANCH", wt.branch.clone()));
    }
    if let Some(slug) = crate::git::get_remote_slug(Path::new(&instance.project_path)) {
        env.push(("AOE_REPO_SLUG", slug));
    }
    env
}

fn format_hook_error(
    cmd: &str,
    exit_code: Option<i32>,
    stderr: &str,
    stdout: &str,
    in_container: bool,
) -> String {
    let prefix = if in_container {
        "Hook command failed in container"
    } else {
        "Hook command failed"
    };
    let mut detail = format!(
        "{} with exit code {}: {}",
        prefix,
        exit_code.unwrap_or(-1),
        cmd
    );
    if !stderr.is_empty() {
        detail.push_str(&format!("\nstderr:\n{}", stderr.trim_end()));
    }
    if !stdout.is_empty() {
        detail.push_str(&format!("\nstdout:\n{}", stdout.trim_end()));
    }
    detail
}

/// Runs `command` with piped output, bounded by the active hook timeout scope.
fn run_hook_output(
    command: &mut std::process::Command,
    cmd: &str,
    spawn_error: impl FnOnce() -> String,
) -> Result<std::process::Output> {
    command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    match crate::session::recovery::current_hook_timeout() {
        None => command.output().with_context(spawn_error),
        Some(deadline) => run_hook_with_timeout(command, deadline, cmd),
    }
}

fn run_hooks_captured(
    commands: &[String],
    target: &HookTarget,
    extra_env: &[(&'static str, String)],
) -> Result<()> {
    for cmd in commands {
        tracing::info!(target: "session.store", "Running hook: {}", cmd);
        let mut command = build_hook_command(cmd, target, HookSpawnOpts::default(), extra_env);
        let output = run_hook_output(&mut command, cmd, || {
            format!("Failed to execute hook: {}", cmd)
        })?;
        if !output.status.success() {
            anyhow::bail!(format_hook_error(
                cmd,
                output.status.code(),
                &String::from_utf8_lossy(&output.stderr),
                &String::from_utf8_lossy(&output.stdout),
                target.in_container()
            ));
        }
        tracing::debug!(target: "session.store",
            "Hook completed: {} (stdout: {} bytes, stderr: {} bytes)",
            cmd,
            output.stdout.len(),
            output.stderr.len()
        );
    }
    Ok(())
}

/// Drains the child's pipes on a thread and, on timeout, kills its process tree
/// so the recovery cascade can release its cross-process lock.
fn run_hook_with_timeout(
    command: &mut std::process::Command,
    timeout: std::time::Duration,
    cmd_label: &str,
) -> Result<std::process::Output> {
    command.stdin(std::process::Stdio::null());
    let child = command
        .spawn()
        .with_context(|| format!("Failed to spawn hook: {}", cmd_label))?;
    let pid = child.id();

    let (tx, rx) = mpsc::channel::<std::io::Result<std::process::Output>>();
    std::thread::Builder::new()
        .name(format!("aoe-hook-drain-{}", pid))
        .spawn(move || {
            let _ = tx.send(child.wait_with_output());
        })
        .expect("hook drain thread spawn");

    match rx.recv_timeout(timeout) {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(io_err)) => {
            // The child may still be live after a wait error.
            crate::process::kill_process_tree(pid);
            Err(anyhow::Error::from(io_err)
                .context(format!("Failed to wait on hook: {}", cmd_label)))
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            tracing::warn!(
                target: "session.startup_recovery",
                cmd = %cmd_label,
                timeout_secs = timeout.as_secs(),
                "hook timed out; killing process tree to release recovery lock"
            );
            crate::process::kill_process_tree(pid);
            Err(anyhow::Error::new(HookTimeout {
                cmd: cmd_label.to_string(),
                timeout_secs: timeout.as_secs(),
            }))
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => anyhow::bail!(
            "hook drain thread disconnected before reporting result: {}",
            cmd_label
        ),
    }
}

fn run_hooks_streamed(
    commands: &[String],
    target: &HookTarget,
    progress_tx: &mpsc::Sender<HookProgress>,
    extra_env: &[(&'static str, String)],
) -> Result<()> {
    use std::io::BufRead;
    // Streamed lines are gone when the failure dialog renders; keep a tail for the error.
    const ERROR_TAIL_LINES: usize = 20;

    for (idx, cmd) in commands.iter().enumerate() {
        tracing::info!(target: "session.store", "Running hook (streamed): {}", cmd);
        let _ = progress_tx.send(HookProgress::Started(cmd.clone()));
        let opts = HookSpawnOpts {
            merge_stderr: true,
            detach_tty: true,
        };
        let mut child = build_hook_command(cmd, target, opts, extra_env)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .with_context(|| format!("Failed to execute hook: {}", cmd))?;

        let mut tail = std::collections::VecDeque::new();
        let mut total_lines = 0usize;
        if let Some(stdout) = child.stdout.take() {
            for line in std::io::BufReader::new(stdout)
                .lines()
                .map_while(Result::ok)
            {
                total_lines += 1;
                if tail.len() == ERROR_TAIL_LINES {
                    tail.pop_front();
                }
                tail.push_back(line.clone());
                let _ = progress_tx.send(HookProgress::Output(line));
            }
        }

        let status = child.wait()?;
        if status.success() {
            tracing::debug!(target: "session.store", "Hook completed (streamed): {}", cmd);
            continue;
        }
        let mut detail = format_hook_error(cmd, status.code(), "", "", target.in_container());
        if !tail.is_empty() {
            let label = if total_lines > tail.len() {
                format!("output (last {} of {} lines)", tail.len(), total_lines)
            } else {
                "output".to_string()
            };
            let lines: Vec<String> = tail.into();
            detail.push_str(&format!("\n{}:\n{}", label, lines.join("\n")));
        }
        if commands.len() > 1 {
            let skipped = if idx + 1 < commands.len() {
                "; remaining hooks skipped"
            } else {
                ""
            };
            detail.push_str(&format!(
                "\n(hook {} of {}{})",
                idx + 1,
                commands.len(),
                skipped
            ));
        }
        let _ = progress_tx.send(HookProgress::Output(detail.clone()));
        anyhow::bail!(detail);
    }
    Ok(())
}

/// `extra_env` is exported to each hook; see `lifecycle_env_vars`.
pub fn execute_hooks(
    commands: &[String],
    project_path: &Path,
    extra_env: &[(&'static str, String)],
) -> Result<()> {
    run_hooks_captured(commands, &HookTarget::Local { project_path }, extra_env)
}

pub fn execute_hooks_in_container(
    commands: &[String],
    container_name: &str,
    workdir: &str,
    extra_env: &[(&'static str, String)],
) -> Result<()> {
    let target = HookTarget::Container {
        container_name,
        workdir,
    };
    run_hooks_captured(commands, &target, extra_env)
}

/// Global+profile only, so a repo can never contribute host commands.
pub fn resolve_before_start_hooks(profile: &str) -> Vec<String> {
    let resolved = super::super::effective_profile(profile);
    profile_config::resolve_config_or_warn(&resolved)
        .host_hooks
        .before_start
}

/// Global+profile only, so a repo can never contribute host commands.
pub fn resolve_before_session_hooks(profile: &str) -> Vec<String> {
    let resolved = super::super::effective_profile(profile);
    profile_config::resolve_config_or_warn(&resolved)
        .host_hooks
        .before_session
}

/// `KEY=VALUE` lines, later keys winning; values are kept verbatim.
fn parse_env_kv_lines(stdout: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for (key, value) in stdout.lines().filter_map(|line| line.split_once('=')) {
        let key = key.trim();
        if !crate::session::environment::is_valid_env_key(key) {
            // Multi-word keys are diagnostics and stay silent. Never log the key:
            // hook stdout is the secret channel.
            if !key.chars().any(char::is_whitespace) {
                tracing::warn!(
                    target: "session.create",
                    "hook produced an invalid environment key; skipping"
                );
            }
            continue;
        }
        out.retain(|(k, _)| k != key);
        out.push((key.to_string(), value.to_string()));
    }
    out
}

/// Runs `host_hooks.before_start` and collects its `KEY=VALUE` pairs.
/// `session_env` (the sandbox environment) overrides inherited env for the hook.
pub fn run_before_start_hooks(
    commands: &[String],
    project_path: &Path,
    extra_env: &[(&'static str, String)],
    session_env: &[(String, String)],
) -> Result<Vec<(String, String)>> {
    run_env_minting_hooks(
        commands,
        project_path,
        extra_env,
        session_env,
        "before_start",
    )
}

/// Runs `host_hooks.before_session`, with the same contract as [`run_before_start_hooks`].
pub fn run_before_session_hooks(
    commands: &[String],
    project_path: &Path,
    extra_env: &[(&'static str, String)],
    session_env: &[(String, String)],
) -> Result<Vec<(String, String)>> {
    run_env_minting_hooks(
        commands,
        project_path,
        extra_env,
        session_env,
        "before_session",
    )
}

/// Stdout carries secrets: it is never logged or included in errors.
fn run_env_minting_hooks(
    commands: &[String],
    project_path: &Path,
    extra_env: &[(&'static str, String)],
    session_env: &[(String, String)],
    label: &str,
) -> Result<Vec<(String, String)>> {
    let mut collected: Vec<(String, String)> = Vec::new();
    for cmd in commands {
        tracing::info!(
            target: "session.store",
            "Running {} host hook (stdout not logged): {}",
            label,
            cmd
        );
        let opts = HookSpawnOpts {
            merge_stderr: false,
            detach_tty: true,
        };
        let mut command =
            build_hook_command(cmd, &HookTarget::Local { project_path }, opts, extra_env);
        command.envs(session_env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        let output = run_hook_output(&mut command, cmd, || {
            format!("Failed to execute {} hook: {}", label, cmd)
        })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stderr_detail = if stderr.trim().is_empty() {
                String::new()
            } else {
                format!("\nstderr:\n{}", stderr.trim_end())
            };
            anyhow::bail!(
                "{} hook failed with exit code {}: {}{}",
                label,
                output.status.code().unwrap_or(-1),
                cmd,
                stderr_detail
            );
        }
        for (key, value) in parse_env_kv_lines(&String::from_utf8_lossy(&output.stdout)) {
            collected.retain(|(k, _)| k != &key);
            collected.push((key, value));
        }
    }
    Ok(collected)
}

/// Attempts every command (for teardown) and returns the failures.
fn run_hooks_best_effort(
    commands: &[String],
    target: &HookTarget,
    detach_tty: bool,
    extra_env: &[(&'static str, String)],
) -> Vec<String> {
    let mut errors = Vec::new();
    for cmd in commands {
        tracing::info!(target: "session.store", "Running hook (best-effort): {}", cmd);
        let opts = HookSpawnOpts {
            merge_stderr: false,
            detach_tty,
        };
        let result = build_hook_command(cmd, target, opts, extra_env)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output();
        let err = match result {
            Ok(output) if output.status.success() => {
                tracing::debug!(target: "session.store",
                    "Hook completed: {} (stdout: {} bytes, stderr: {} bytes)",
                    cmd,
                    output.stdout.len(),
                    output.stderr.len()
                );
                continue;
            }
            Ok(output) => format_hook_error(
                cmd,
                output.status.code(),
                &String::from_utf8_lossy(&output.stderr),
                &String::from_utf8_lossy(&output.stdout),
                target.in_container(),
            ),
            Err(e) => format!("Failed to execute hook: {}: {}", cmd, e),
        };
        tracing::warn!(target: "session.store", "{}", err);
        errors.push(err);
    }
    errors
}

/// `detach_tty`: true from TUI/web contexts, false from the CLI so prompts stay answerable.
pub fn execute_hooks_best_effort(
    commands: &[String],
    project_path: &Path,
    detach_tty: bool,
    extra_env: &[(&'static str, String)],
) -> Vec<String> {
    let target = HookTarget::Local { project_path };
    run_hooks_best_effort(commands, &target, detach_tty, extra_env)
}

/// `detach_tty`: true from TUI/web contexts, false from the CLI so prompts stay answerable.
pub fn execute_hooks_in_container_best_effort(
    commands: &[String],
    container_name: &str,
    workdir: &str,
    detach_tty: bool,
    extra_env: &[(&'static str, String)],
) -> Vec<String> {
    let target = HookTarget::Container {
        container_name,
        workdir,
    };
    run_hooks_best_effort(commands, &target, detach_tty, extra_env)
}

pub fn execute_hooks_streamed(
    commands: &[String],
    project_path: &Path,
    progress_tx: &mpsc::Sender<HookProgress>,
    extra_env: &[(&'static str, String)],
) -> Result<()> {
    let target = HookTarget::Local { project_path };
    run_hooks_streamed(commands, &target, progress_tx, extra_env)
}

pub fn execute_hooks_in_container_streamed(
    commands: &[String],
    container_name: &str,
    workdir: &str,
    progress_tx: &mpsc::Sender<HookProgress>,
    extra_env: &[(&'static str, String)],
) -> Result<()> {
    let target = HookTarget::Container {
        container_name,
        workdir,
    };
    run_hooks_streamed(commands, &target, progress_tx, extra_env)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins `SHELL` to a resolvable `sh` (and takes the env lock) so local hooks
    /// cannot fail on an ambient or concurrently changing `SHELL` (#3449).
    #[must_use = "bind it to `_shell`; dropped immediately, it unpins SHELL again"]
    fn pin_host_shell() -> Option<crate::session::test_support::EnvGuard> {
        let guard = crate::session::test_support::EnvGuard::read_lock();
        let Ok(sh) = which::which("sh") else {
            eprintln!("not pinning SHELL: sh not found on PATH");
            return None;
        };
        Some(guard.and_set("SHELL", &sh))
    }

    fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn cmds(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn container_args(opts: HookSpawnOpts, env: &[(&'static str, String)]) -> Vec<String> {
        let target = HookTarget::Container {
            container_name: "test_container",
            workdir: "/work",
        };
        build_hook_command("true", &target, opts, env)
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn parse_env_kv_lines_filters_later_wins_and_never_logs_a_malformed_key() {
        let logs = crate::session::test_support::LogCapture::start();
        assert_eq!(
            parse_env_kv_lines(
                "minting...\n\nGH_TOKEN=a=b=c\n9BAD=x\nNO_EQUALS\n  SPACED  =v\nK=first\r\nK=second\r\n"
            ),
            pairs(&[("GH_TOKEN", "a=b=c"), ("SPACED", "v"), ("K", "second")])
        );
        assert!(parse_env_kv_lines("https://token:topsecret@example.test?x=ignored\n").is_empty());
        let logs = logs.contents();
        assert!(logs.contains("invalid environment key"), "{logs}");
        assert!(!logs.contains("topsecret"), "{logs}");
    }

    #[test]
    fn env_minting_hooks_collect_pairs_from_env_and_session() {
        let _shell = pin_host_shell();
        let tmp = tempfile::tempdir().unwrap();
        let minted = run_before_start_hooks(
            &cmds(&[
                "echo \"GH_TOKEN=tok-$TEST_VAR\"",
                "printf 'FOO=bar\\nnoise line\\n'",
            ]),
            tmp.path(),
            &[],
            &pairs(&[("TEST_VAR", "alpha")]),
        )
        .unwrap();
        assert_eq!(minted, pairs(&[("GH_TOKEN", "tok-alpha"), ("FOO", "bar")]));

        let minted = run_before_session_hooks(
            &cmds(&["echo \"CLAUDE_CONFIG_DIR=/accounts/$AOE_PROFILE\""]),
            tmp.path(),
            &[("AOE_PROFILE", "work".to_string())],
            &[],
        )
        .unwrap();
        assert_eq!(minted, pairs(&[("CLAUDE_CONFIG_DIR", "/accounts/work")]));
    }

    #[test]
    fn env_minting_hook_errors_name_the_hook_and_omit_stdout() {
        let _shell = pin_host_shell();
        let tmp = tempfile::tempdir().unwrap();
        let extra_env = [("SECRET_SRC", "topsecret".to_string())];
        let failing = cmds(&["echo \"TOKEN=$SECRET_SRC\"; echo boom 1>&2; exit 4"]);
        type Runner = fn(
            &[String],
            &Path,
            &[(&'static str, String)],
            &[(String, String)],
        ) -> Result<Vec<(String, String)>>;
        let runners: [(&str, Runner); 2] = [
            ("before_start", run_before_start_hooks),
            ("before_session", run_before_session_hooks),
        ];
        for (label, run) in runners {
            let msg = run(&failing, tmp.path(), &extra_env, &[])
                .expect_err("non-zero exit must be an error")
                .to_string();
            assert!(msg.starts_with(label), "got: {msg}");
            assert!(
                msg.contains("exit code 4") && msg.contains("boom"),
                "got: {msg}"
            );
            assert!(!msg.contains("topsecret"), "stdout secret leaked: {msg}");
        }
    }

    #[test]
    fn origin_hint_names_the_layer_the_commands_came_from() {
        use super::super::{LEGACY_REPO_CONFIG_PATH, REPO_CONFIG_PATH};
        let _app = crate::session::test_support::isolate_app_dir();
        let write = |path: &Path, body: &str| {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        };
        let repo_with = |rel: &str, body: &str| {
            let dir = tempfile::TempDir::new().unwrap();
            write(&dir.path().join(rel), body);
            let file = dir.path().join(rel);
            (dir, file)
        };
        let (repo, repo_file) =
            repo_with(REPO_CONFIG_PATH, "[hooks]\non_create = [\"repo-cmd\"]\n");
        let (legacy, legacy_file) = repo_with(
            LEGACY_REPO_CONFIG_PATH,
            "[hooks]\non_create = [\"legacy-cmd\"]\n",
        );
        let (launch_only, _) = repo_with(REPO_CONFIG_PATH, "[hooks]\non_launch = [\"x\"]\n");
        let (same_as_global, same_file) =
            repo_with(REPO_CONFIG_PATH, "[hooks]\non_create = [\"global-cmd\"]\n");
        let global = crate::session::get_app_dir_path()
            .unwrap()
            .join("config.toml");
        write(&global, "[hooks]\non_create = [\"global-cmd\"]\n");
        let profile = crate::session::get_profile_dir_path("work")
            .unwrap()
            .join("config.toml");
        write(&profile, "[hooks]\non_create = [\"profile-cmd\"]\n");

        let repo_run = |root: &Path| {
            let hooks = super::super::load_repo_config(root)
                .unwrap()
                .unwrap()
                .hooks()
                .unwrap();
            ResolvedHooks::with_repo("default", root, hooks).unwrap()
        };
        let named = |path: &Path| {
            Some(format!(
                "declared in {} ([hooks] on_create)",
                path.display()
            ))
        };
        let cases: Vec<(&str, ResolvedHooks, &str, Option<String>)> = vec![
            (
                "trusted repo",
                repo_run(repo.path()),
                "on_create",
                named(&repo_file),
            ),
            (
                "legacy layout",
                repo_run(legacy.path()),
                "on_create",
                named(&legacy_file),
            ),
            (
                "repo merged, type inherited",
                repo_run(launch_only.path()),
                "on_create",
                named(&global),
            ),
            (
                "repo repeats global",
                repo_run(same_as_global.path()),
                "on_create",
                named(&same_file),
            ),
            // Declined trust: global ran, although a repo on disk declares the same commands.
            (
                "repo not merged",
                ResolvedHooks::global("default").unwrap(),
                "on_create",
                named(&global),
            ),
            ("unknown type", repo_run(repo.path()), "on_explode", None),
            ("empty type", repo_run(repo.path()), "on_destroy", None),
            (
                "matches no layer",
                ResolvedHooks {
                    hooks: HooksConfig {
                        on_create: cmds(&["ghost"]),
                        ..Default::default()
                    },
                    profile: "default".into(),
                    repo_root: None,
                },
                "on_create",
                None,
            ),
        ];
        for (label, run, hook_type, expected) in cases {
            assert_eq!(run.origin_hint(hook_type), expected, "{label}");
        }

        let work = ResolvedHooks::global("work").unwrap();
        assert_eq!(work.origin_hint("on_create"), named(&profile));

        let unseen = crate::session::get_profile_dir_path("unseen").unwrap();
        let unseen_run = ResolvedHooks::global("unseen").unwrap();
        assert_eq!(unseen_run.origin_hint("on_create"), named(&global));
        assert!(!unseen.exists(), "the hint must not create a profile dir");
    }

    #[test]
    fn hook_display_groups_label_source_and_filter() {
        let repo = HooksConfig {
            on_create: cmds(&["repo-create"]),
            on_destroy: cmds(&["repo-destroy"]),
            ..Default::default()
        };
        let merged = HooksConfig {
            on_launch: cmds(&["global-launch"]),
            ..repo.clone()
        };
        let summary = |include_destroy| {
            hook_display_groups(&merged, &repo, include_destroy)
                .iter()
                .map(|g| (g.name, g.source_label(), g.commands.clone()))
                .collect::<Vec<_>>()
        };
        let mut expected = vec![
            ("on_create", " (from repo)", cmds(&["repo-create"])),
            (
                "on_launch",
                " (from global config)",
                cmds(&["global-launch"]),
            ),
        ];
        assert_eq!(summary(false), expected);
        expected.push(("on_destroy", " (from repo)", cmds(&["repo-destroy"])));
        assert_eq!(summary(true), expected);
    }

    /// #901: streamed hooks detach from the TUI terminal and defang git/ssh prompts.
    #[test]
    fn streamed_hook_detached_from_tty() {
        let _shell = pin_host_shell();
        let tmp = tempfile::tempdir().unwrap();
        let probe = r#"
            if [ -t 0 ]; then echo "STDIN=tty"; else echo "STDIN=notty"; fi
            echo "GIT_TERMINAL_PROMPT=${GIT_TERMINAL_PROMPT:-unset}"
            echo "GIT_ASKPASS=${GIT_ASKPASS:-unset}"
            echo "SSH_ASKPASS=${SSH_ASKPASS:-unset}"
        "#;
        let (tx, rx) = mpsc::channel();
        execute_hooks_streamed(&cmds(&[probe]), tmp.path(), &tx, &[]).unwrap();
        drop(tx);
        let lines: Vec<String> = rx
            .into_iter()
            .filter_map(|p| match p {
                HookProgress::Output(line) => Some(line),
                HookProgress::Started(_) => None,
            })
            .collect();
        assert_eq!(
            lines,
            cmds(&[
                "STDIN=notty",
                "GIT_TERMINAL_PROMPT=0",
                "GIT_ASKPASS=true",
                "SSH_ASKPASS=true"
            ])
        );
    }

    /// The returned error is all the creation dialog sees, so it carries the output and position.
    #[test]
    fn streamed_hook_failure_error_includes_output_and_position() {
        let _shell = pin_host_shell();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("hook.sh"),
            "#!/bin/sh\necho 'fatal: dependency xyz not found' >&2\nexit 3\n",
        )
        .unwrap();
        let (tx, _rx) = mpsc::channel();
        let error = |hooks: &[&str]| {
            format!(
                "{:#}",
                execute_hooks_streamed(&cmds(hooks), tmp.path(), &tx, &[]).unwrap_err()
            )
        };
        let msg = error(&["sh hook.sh"]);
        assert!(msg.contains("exit code 3"), "got: {msg}");
        assert!(
            msg.contains("fatal: dependency xyz not found"),
            "got: {msg}"
        );
        assert!(!msg.contains("(hook "), "got: {msg}");
        let msg = error(&["true", "sh -c 'exit 7'", "true"]);
        assert!(
            msg.contains("(hook 2 of 3; remaining hooks skipped)"),
            "got: {msg}"
        );
        let msg = error(&["true", "sh -c 'exit 7'"]);
        assert!(
            msg.contains("(hook 2 of 2)") && !msg.contains("skipped"),
            "got: {msg}"
        );
    }

    /// CLI paths keep the terminal attached; TUI/web best-effort paths detach.
    #[test]
    #[serial_test::serial]
    fn captured_and_best_effort_hooks_detach_only_when_requested() {
        let _env = crate::session::test_support::EnvGuard::unset(&["GIT_TERMINAL_PROMPT"]);
        let _shell = pin_host_shell();
        let tmp = tempfile::tempdir().unwrap();
        let probe = cmds(&["echo \"GIT_TERMINAL_PROMPT=${GIT_TERMINAL_PROMPT:-unset}\" > out.txt"]);
        let out = || std::fs::read_to_string(tmp.path().join("out.txt")).unwrap();

        execute_hooks(&probe, tmp.path(), &[]).unwrap();
        assert!(out().contains("=unset"), "captured: {}", out());
        for (detach, expected) in [(true, "=0"), (false, "=unset")] {
            let errors = execute_hooks_best_effort(&probe, tmp.path(), detach, &[]);
            assert!(errors.is_empty(), "{errors:?}");
            assert!(out().contains(expected), "detach {detach}: {}", out());
        }
    }

    #[test]
    fn container_hook_forwards_env_as_single_dash_e_args() {
        let has_pair =
            |args: &[String], pair: &str| args.windows(2).any(|w| w[0] == "-e" && w[1] == pair);
        let detached = container_args(
            HookSpawnOpts {
                merge_stderr: true,
                detach_tty: true,
            },
            &[],
        );
        for pair in [
            "GIT_TERMINAL_PROMPT=0",
            "GIT_ASKPASS=true",
            "SSH_ASKPASS=true",
        ] {
            assert!(has_pair(&detached, pair), "{pair}: {detached:?}");
        }
        let attached = container_args(HookSpawnOpts::default(), &[]);
        assert!(!attached.iter().any(|a| a == "-e"), "{attached:?}");

        let env = [
            ("AOE_SESSION_ID", "s_abc".to_string()),
            ("AOE_SESSION_TITLE", "My Title".to_string()),
        ];
        let args = container_args(HookSpawnOpts::default(), &env);
        assert!(has_pair(&args, "AOE_SESSION_ID=s_abc"), "{args:?}");
        assert!(has_pair(&args, "AOE_SESSION_TITLE=My Title"), "{args:?}");
    }

    #[test]
    fn lifecycle_env_vars_shape() {
        use crate::session::instance::WorktreeInfo;
        use crate::session::Instance;

        let mut instance = Instance::new("My Session", "/tmp/proj");
        instance.tool = "claude".to_string();
        instance.group_path = "backend/api".to_string();
        instance.source_profile = "work".to_string();
        let env = |instance: &Instance| -> std::collections::HashMap<_, _> {
            lifecycle_env_vars(instance).into_iter().collect()
        };
        let vars = env(&instance);
        for (key, value) in [
            ("AOE_SESSION_ID", instance.id.as_str()),
            ("AOE_SESSION_TITLE", "My Session"),
            ("AOE_PROJECT_PATH", "/tmp/proj"),
            ("AOE_TOOL", "claude"),
            ("AOE_GROUP_PATH", "backend/api"),
        ] {
            assert_eq!(vars.get(key).map(String::as_str), Some(value), "{key}");
        }
        assert!(vars.contains_key("AOE_PROFILE") && !vars.contains_key("AOE_SESSION_BRANCH"));

        instance.worktree_info = Some(WorktreeInfo {
            branch: "feature/auth".to_string(),
            main_repo_path: "/tmp/proj".to_string(),
            managed_by_aoe: true,
            created_at: chrono::Utc::now(),
            base_branch: None,
        });
        assert_eq!(
            env(&instance).get("AOE_SESSION_BRANCH").map(String::as_str),
            Some("feature/auth")
        );
    }

    /// Session env reaches the captured, best-effort and streamed local paths.
    #[test]
    fn local_hooks_see_session_env_vars() {
        let _shell = pin_host_shell();
        let tmp = tempfile::tempdir().unwrap();
        let mut instance = crate::session::Instance::new("My Title", tmp.path().to_str().unwrap());
        instance.tool = "codex".to_string();
        let env = lifecycle_env_vars(&instance);
        let probe = cmds(&[
            "echo \"ID=${AOE_SESSION_ID} TITLE=${AOE_SESSION_TITLE} TOOL=${AOE_TOOL}\" > env.txt",
        ]);
        let expected = format!("ID={} TITLE=My Title TOOL=codex\n", instance.id);
        let out = || std::fs::read_to_string(tmp.path().join("env.txt")).unwrap();

        execute_hooks(&probe, tmp.path(), &env).unwrap();
        assert_eq!(out(), expected, "captured");
        std::fs::remove_file(tmp.path().join("env.txt")).unwrap();
        assert!(execute_hooks_best_effort(&probe, tmp.path(), false, &env).is_empty());
        assert_eq!(out(), expected, "best-effort");
        let (tx, _rx) = mpsc::channel();
        std::fs::remove_file(tmp.path().join("env.txt")).unwrap();
        execute_hooks_streamed(&probe, tmp.path(), &tx, &env).unwrap();
        assert_eq!(out(), expected, "streamed");
    }
}
