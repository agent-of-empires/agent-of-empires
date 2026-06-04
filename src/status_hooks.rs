//! Local command hooks for session status transitions.

use std::collections::HashMap;
#[cfg(not(test))]
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use aoe_settings_derive::SettingsSection;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::session::{Instance, Status};

const DEFAULT_DEBOUNCE_MS: u64 = 100;

#[derive(Debug, Clone, Serialize, Deserialize, SettingsSection)]
#[setting_section(name = "status_hooks", category = "Status Hooks")]
pub struct StatusHookConfig {
    /// Run local commands when TUI sessions change status.
    #[serde(default)]
    #[setting(label = "Enabled", widget = "toggle")]
    pub enabled: bool,

    /// Milliseconds a status must remain stable before running hook commands.
    /// Always serialized (no skip-at-default) so every surface that reads the
    /// config JSON, including the settings schema consumers, sees a concrete
    /// value rather than an absent leaf that would display as 0 (#1692).
    #[serde(default = "default_debounce_ms")]
    #[setting(label = "Debounce (ms)", widget = "number", min = 0)]
    pub debounce_ms: u64,

    /// Shell command run when a session enters Starting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[setting(
        label = "On Starting",
        widget = "optional_text",
        web = "local_only:runs a local shell command on status change, a host execution surface"
    )]
    pub on_starting: Option<String>,

    /// Shell command run when a session enters Running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[setting(
        label = "On Running",
        widget = "optional_text",
        web = "local_only:runs a local shell command on status change, a host execution surface"
    )]
    pub on_running: Option<String>,

    /// Shell command run when a session enters Waiting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[setting(
        label = "On Waiting",
        widget = "optional_text",
        web = "local_only:runs a local shell command on status change, a host execution surface"
    )]
    pub on_waiting: Option<String>,

    /// Shell command run when a session enters Idle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[setting(
        label = "On Idle",
        widget = "optional_text",
        web = "local_only:runs a local shell command on status change, a host execution surface"
    )]
    pub on_idle: Option<String>,

    /// Shell command run when a session enters Error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[setting(
        label = "On Error",
        widget = "optional_text",
        web = "local_only:runs a local shell command on status change, a host execution surface"
    )]
    pub on_error: Option<String>,

    /// Shell command run after the status-specific command on every status
    /// change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[setting(
        label = "On Any Change",
        widget = "optional_text",
        web = "local_only:runs a local shell command on status change, a host execution surface"
    )]
    pub on_change: Option<String>,
}

impl Default for StatusHookConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            debounce_ms: DEFAULT_DEBOUNCE_MS,
            on_starting: None,
            on_running: None,
            on_waiting: None,
            on_idle: None,
            on_error: None,
            on_change: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusHookContext {
    pub session_id: String,
    pub session_title: String,
    pub project_path: String,
    pub profile: String,
    pub tool: String,
    pub group_path: String,
    pub old_status: Status,
    pub new_status: Status,
    pub changed_at: DateTime<Utc>,
}

impl StatusHookContext {
    pub fn from_instance(
        instance: &Instance,
        old_status: Status,
        new_status: Status,
        changed_at: DateTime<Utc>,
    ) -> Self {
        Self {
            session_id: instance.id.clone(),
            session_title: instance.title.clone(),
            project_path: instance.project_path.clone(),
            profile: instance.effective_profile(),
            tool: instance.tool.clone(),
            group_path: instance.group_path.clone(),
            old_status,
            new_status,
            changed_at,
        }
    }

    pub fn env_vars(&self) -> [(&'static str, String); 9] {
        [
            ("AOE_SESSION_ID", self.session_id.clone()),
            ("AOE_SESSION_TITLE", self.session_title.clone()),
            ("AOE_PROJECT_PATH", self.project_path.clone()),
            ("AOE_PROFILE", self.profile.clone()),
            ("AOE_TOOL", self.tool.clone()),
            ("AOE_GROUP_PATH", self.group_path.clone()),
            ("AOE_OLD_STATUS", self.old_status.as_str().to_string()),
            ("AOE_NEW_STATUS", self.new_status.as_str().to_string()),
            ("AOE_STATUS_CHANGED_AT", self.changed_at.to_rfc3339()),
        ]
    }
}

pub fn commands_for_transition(old: Status, new: Status, config: &StatusHookConfig) -> Vec<String> {
    if !config.enabled || old == new {
        return Vec::new();
    }

    let mut commands = Vec::new();
    let specific = match new {
        Status::Starting => config.on_starting.as_deref(),
        Status::Running => config.on_running.as_deref(),
        Status::Waiting => config.on_waiting.as_deref(),
        Status::Idle => config.on_idle.as_deref(),
        Status::Error => config.on_error.as_deref(),
        Status::Unknown | Status::Stopped | Status::Deleting | Status::Creating => None,
    };
    if let Some(cmd) = non_empty_command(specific) {
        commands.push(cmd.to_string());
    }
    if let Some(cmd) = non_empty_command(config.on_change.as_deref()) {
        commands.push(cmd.to_string());
    }
    commands
}

pub fn has_configured_commands(config: &StatusHookConfig) -> bool {
    config.enabled
        && [
            config.on_starting.as_deref(),
            config.on_running.as_deref(),
            config.on_waiting.as_deref(),
            config.on_idle.as_deref(),
            config.on_error.as_deref(),
            config.on_change.as_deref(),
        ]
        .into_iter()
        .any(|cmd| non_empty_command(cmd).is_some())
}

pub fn run_for_transition(
    instance: &Instance,
    old: Status,
    new: Status,
    config: &StatusHookConfig,
) {
    if !config.enabled || old == new {
        return;
    }

    let changed_at = Utc::now();
    let commands = commands_for_transition(old, new, config);
    if config.debounce_ms > 0 {
        run_debounced_transition(instance, old, new, changed_at, commands, config.debounce_ms);
        return;
    }

    if commands.is_empty() {
        return;
    }
    spawn_transition_commands(instance, old, new, changed_at, commands);
}

fn non_empty_command(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|s| !s.is_empty())
}

fn default_debounce_ms() -> u64 {
    DEFAULT_DEBOUNCE_MS
}

fn spawn_transition_commands(
    instance: &Instance,
    old: Status,
    new: Status,
    changed_at: DateTime<Utc>,
    commands: Vec<String>,
) {
    let context = StatusHookContext::from_instance(instance, old, new, changed_at);
    // Keep one transition's commands in one worker so `on_change` cannot race
    // ahead of the status-specific hook.
    spawn_hook_commands(commands, context);
}

#[derive(Debug, Clone)]
struct DebounceEntry {
    stable_status: Status,
    generation: u64,
    pending_status: Option<Status>,
}

fn debounce_state() -> &'static Mutex<HashMap<String, DebounceEntry>> {
    static STATE: OnceLock<Mutex<HashMap<String, DebounceEntry>>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn run_debounced_transition(
    instance: &Instance,
    old: Status,
    new: Status,
    changed_at: DateTime<Utc>,
    commands: Vec<String>,
    debounce_ms: u64,
) {
    let session_id = instance.id.clone();
    let mut state = debounce_state().lock().unwrap();
    let entry = state.entry(session_id.clone()).or_insert(DebounceEntry {
        stable_status: old,
        generation: 0,
        pending_status: None,
    });
    entry.generation = entry.generation.wrapping_add(1);
    let generation = entry.generation;
    let stable_status = entry.stable_status;

    if new == stable_status {
        entry.pending_status = None;
        return;
    }

    if commands.is_empty() {
        entry.stable_status = new;
        entry.pending_status = None;
        return;
    }

    entry.pending_status = Some(new);
    drop(state);

    let instance = instance.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(debounce_ms));
        let mut state = debounce_state().lock().unwrap();
        let should_run = match state.get_mut(&session_id) {
            Some(entry) if entry.generation == generation && entry.pending_status == Some(new) => {
                entry.stable_status = new;
                entry.pending_status = None;
                true
            }
            _ => false,
        };
        drop(state);

        if should_run {
            spawn_transition_commands(&instance, stable_status, new, changed_at, commands);
        }
    });
}

#[cfg(not(test))]
fn spawn_hook_commands(commands: Vec<String>, context: StatusHookContext) {
    std::thread::spawn(move || {
        let project_path = PathBuf::from(&context.project_path);
        for command in commands {
            let result = run_hook_command_blocking(&command, &context, &project_path);
            if let Err(e) = result {
                tracing::warn!(
                    target: "hooks.status_hooks",
                    session_id = %context.session_id,
                    new_status = %context.new_status.as_str(),
                    "status hook failed: {}",
                    e
                );
            }
        }
    });
}

#[cfg(test)]
fn spawn_hook_commands(commands: Vec<String>, context: StatusHookContext) {
    let mut launches = recorded_launches().lock().unwrap();
    for command in commands {
        launches.push(RecordedLaunch {
            command,
            context: context.clone(),
        });
    }
}

/// Upper bound on how long a single status hook may block its worker
/// thread. A misconfigured hook (e.g. one that opens a foreground GUI
/// app, hangs on stdin, or `tail -f`s a log) would otherwise leak the
/// std::thread spawned in `spawn_hook_commands` for the life of the
/// TUI. 30s is generous for sound players and notifier CLIs, short
/// enough that a steady stream of long-stuck hooks doesn't accumulate
/// indefinitely.
#[cfg(not(test))]
const HOOK_COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

#[cfg(not(test))]
fn run_hook_command_blocking(
    command: &str,
    context: &StatusHookContext,
    project_path: &Path,
) -> std::io::Result<()> {
    let mut child = build_command(command, context, project_path).spawn()?;
    let deadline = std::time::Instant::now() + HOOK_COMMAND_TIMEOUT;
    loop {
        match child.try_wait()? {
            Some(status) => {
                return if status.success() {
                    Ok(())
                } else {
                    Err(std::io::Error::other(format!(
                        "command exited with status {:?}",
                        status.code()
                    )))
                };
            }
            None => {
                if std::time::Instant::now() >= deadline {
                    // Best-effort kill; if the child has already exited
                    // between the try_wait above and here, kill is a
                    // no-op error we don't care about.
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(std::io::Error::other(format!(
                        "command timed out after {}s",
                        HOOK_COMMAND_TIMEOUT.as_secs()
                    )));
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }
}

#[cfg(not(test))]
fn build_command(
    command: &str,
    context: &StatusHookContext,
    project_path: &Path,
) -> std::process::Command {
    let mut child = std::process::Command::new(crate::session::user_shell());
    child
        .arg("-c")
        .arg(command)
        .current_dir(project_path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "true")
        .env("SSH_ASKPASS", "true");
    for (key, value) in context.env_vars() {
        child.env(key, value);
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            child.pre_exec(|| {
                nix::unistd::setsid().map_err(std::io::Error::other)?;
                Ok(())
            });
        }
    }

    child
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedLaunch {
    pub command: String,
    pub context: StatusHookContext,
}

#[cfg(test)]
fn recorded_launches() -> &'static std::sync::Mutex<Vec<RecordedLaunch>> {
    static LAUNCHES: std::sync::OnceLock<std::sync::Mutex<Vec<RecordedLaunch>>> =
        std::sync::OnceLock::new();
    LAUNCHES.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

#[cfg(test)]
pub fn take_recorded_launches() -> Vec<RecordedLaunch> {
    std::mem::take(&mut *recorded_launches().lock().unwrap())
}

#[cfg(test)]
pub fn reset_debounce_state() {
    debounce_state().lock().unwrap().clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::time::Instant;

    fn wait_for_recorded_launch_count(expected: usize) {
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            if recorded_launches().lock().unwrap().len() == expected {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn default_config_is_disabled() {
        let config = StatusHookConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.debounce_ms, DEFAULT_DEBOUNCE_MS);
        assert!(commands_for_transition(Status::Running, Status::Waiting, &config).is_empty());
    }

    #[test]
    fn deserializes_toml_config() {
        let config: StatusHookConfig = toml::from_str(
            r#"
            enabled = true
            on_waiting = "notify-send waiting"
            on_change = "~/bin/aoe-hook"
            "#,
        )
        .unwrap();
        assert!(config.enabled);
        assert_eq!(config.debounce_ms, DEFAULT_DEBOUNCE_MS);
        assert_eq!(config.on_waiting.as_deref(), Some("notify-send waiting"));
        assert_eq!(config.on_change.as_deref(), Some("~/bin/aoe-hook"));
    }

    #[test]
    fn deserializes_debounce_ms() {
        let config: StatusHookConfig = toml::from_str(
            r#"
            enabled = true
            debounce_ms = 500
            on_waiting = "notify-send waiting"
            "#,
        )
        .unwrap();
        assert_eq!(config.debounce_ms, 500);
    }

    #[test]
    fn resolves_specific_command_before_catch_all() {
        let config = StatusHookConfig {
            enabled: true,
            on_waiting: Some("waiting-command".to_string()),
            on_change: Some("change-command".to_string()),
            ..Default::default()
        };
        assert_eq!(
            commands_for_transition(Status::Running, Status::Waiting, &config),
            vec!["waiting-command".to_string(), "change-command".to_string()]
        );
    }

    #[test]
    fn skips_empty_commands_and_same_status() {
        let config = StatusHookConfig {
            enabled: true,
            on_waiting: Some("  ".to_string()),
            on_change: Some("change-command".to_string()),
            ..Default::default()
        };
        assert!(commands_for_transition(Status::Waiting, Status::Waiting, &config).is_empty());
        assert_eq!(
            commands_for_transition(Status::Running, Status::Waiting, &config),
            vec!["change-command".to_string()]
        );
    }

    #[test]
    #[serial]
    fn debounces_stable_transition() {
        reset_debounce_state();
        take_recorded_launches();

        let mut instance = Instance::new("Debounce Stable", "/tmp/project");
        instance.id = "debounce-stable".to_string();
        let config = StatusHookConfig {
            enabled: true,
            debounce_ms: 10,
            on_waiting: Some("notify-waiting".to_string()),
            ..Default::default()
        };

        let observed_before = Utc::now();
        run_for_transition(&instance, Status::Running, Status::Waiting, &config);
        let observed_after = Utc::now();
        assert!(take_recorded_launches().is_empty());

        wait_for_recorded_launch_count(1);
        let launches = take_recorded_launches();
        assert_eq!(launches.len(), 1);
        assert_eq!(launches[0].command, "notify-waiting");
        assert_eq!(launches[0].context.old_status, Status::Running);
        assert_eq!(launches[0].context.new_status, Status::Waiting);
        assert!(launches[0].context.changed_at >= observed_before);
        assert!(launches[0].context.changed_at <= observed_after);
    }

    #[test]
    #[serial]
    fn debounce_cancels_flicker_back_to_stable_status() {
        reset_debounce_state();
        take_recorded_launches();

        let mut instance = Instance::new("Debounce Flicker", "/tmp/project");
        instance.id = "debounce-flicker".to_string();
        let config = StatusHookConfig {
            enabled: true,
            debounce_ms: 10,
            on_waiting: Some("notify-waiting".to_string()),
            ..Default::default()
        };

        run_for_transition(&instance, Status::Running, Status::Waiting, &config);
        run_for_transition(&instance, Status::Waiting, Status::Running, &config);

        std::thread::sleep(Duration::from_millis(30));
        assert!(take_recorded_launches().is_empty());
    }

    #[test]
    #[serial]
    fn debounce_coalesces_to_latest_pending_status() {
        reset_debounce_state();
        take_recorded_launches();

        let mut instance = Instance::new("Debounce Latest", "/tmp/project");
        instance.id = "debounce-latest".to_string();
        let config = StatusHookConfig {
            enabled: true,
            debounce_ms: 10,
            on_waiting: Some("notify-waiting".to_string()),
            on_idle: Some("notify-idle".to_string()),
            ..Default::default()
        };

        run_for_transition(&instance, Status::Running, Status::Waiting, &config);
        run_for_transition(&instance, Status::Waiting, Status::Idle, &config);

        wait_for_recorded_launch_count(1);
        let launches = take_recorded_launches();
        assert_eq!(launches.len(), 1);
        assert_eq!(launches[0].command, "notify-idle");
        assert_eq!(launches[0].context.old_status, Status::Running);
        assert_eq!(launches[0].context.new_status, Status::Idle);
    }

    #[test]
    fn builds_context_env_vars() {
        let mut instance = Instance::new("Build API", "/tmp/project");
        instance.id = "abc123".to_string();
        instance.tool = "codex".to_string();
        instance.group_path = "Backend".to_string();
        instance.source_profile = "work".to_string();
        let changed_at = DateTime::parse_from_rfc3339("2026-05-20T10:11:12Z")
            .unwrap()
            .with_timezone(&Utc);
        let context = StatusHookContext::from_instance(
            &instance,
            Status::Running,
            Status::Waiting,
            changed_at,
        );
        let env = context.env_vars();
        assert!(env.contains(&("AOE_SESSION_ID", "abc123".to_string())));
        assert!(env.contains(&("AOE_SESSION_TITLE", "Build API".to_string())));
        assert!(env.contains(&("AOE_PROJECT_PATH", "/tmp/project".to_string())));
        assert!(env.contains(&("AOE_PROFILE", "work".to_string())));
        assert!(env.contains(&("AOE_TOOL", "codex".to_string())));
        assert!(env.contains(&("AOE_GROUP_PATH", "Backend".to_string())));
        assert!(env.contains(&("AOE_OLD_STATUS", "running".to_string())));
        assert!(env.contains(&("AOE_NEW_STATUS", "waiting".to_string())));
        assert!(env.contains(&(
            "AOE_STATUS_CHANGED_AT",
            "2026-05-20T10:11:12+00:00".to_string()
        )));
    }
}
