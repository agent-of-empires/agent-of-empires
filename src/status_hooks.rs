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

/// A status must stay stable this long before hooks run, so flickers do not fire them.
#[cfg(not(test))]
const DEFAULT_DEBOUNCE_MS: u64 = 100;

#[cfg(test)]
static TEST_DEBOUNCE_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(test)]
pub fn set_test_debounce_ms(ms: u64) {
    TEST_DEBOUNCE_MS.store(ms, std::sync::atomic::Ordering::SeqCst);
}

fn effective_debounce_ms() -> u64 {
    #[cfg(test)]
    {
        TEST_DEBOUNCE_MS.load(std::sync::atomic::Ordering::SeqCst)
    }
    #[cfg(not(test))]
    {
        DEFAULT_DEBOUNCE_MS
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, SettingsSection)]
#[setting_section(name = "status_hooks", category = "Status Hooks")]
pub struct StatusHookConfig {
    /// Run local commands when TUI sessions change status.
    #[serde(default)]
    #[setting(label = "Enabled", widget = "toggle")]
    pub enabled: bool,

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
    let debounce_ms = effective_debounce_ms();
    if debounce_ms > 0 {
        run_debounced_transition(instance, old, new, changed_at, commands, debounce_ms);
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

fn spawn_transition_commands(
    instance: &Instance,
    old: Status,
    new: Status,
    changed_at: DateTime<Utc>,
    commands: Vec<String>,
) {
    let context = StatusHookContext::from_instance(instance, old, new, changed_at);
    // One worker per transition so `on_change` cannot race ahead of the status hook.
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
    #[cfg(test)]
    let gate = DEBOUNCE_WORKERS.with(|workers| {
        workers
            .borrow()
            .as_ref()
            .map(|_| std::sync::mpsc::channel::<()>())
    });
    #[cfg(test)]
    let (release, wait) = gate.map_or((None, None), |(tx, rx)| (Some(tx), Some(rx)));
    let worker = std::thread::spawn(move || {
        #[cfg(test)]
        if let Some(wait) = wait {
            let _ = wait.recv();
        } else {
            std::thread::sleep(Duration::from_millis(debounce_ms));
        }
        #[cfg(not(test))]
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
    #[cfg(test)]
    if let Some(release) = release {
        DEBOUNCE_WORKERS.with(|workers| {
            workers
                .borrow_mut()
                .as_mut()
                .unwrap()
                .push((release, worker));
        });
    }
    #[cfg(not(test))]
    drop(worker);
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

/// Bounds how long a stuck hook can hold its worker thread.
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
type DebounceWorker = (std::sync::mpsc::Sender<()>, std::thread::JoinHandle<()>);

#[cfg(test)]
thread_local! {
    static DEBOUNCE_WORKERS: std::cell::RefCell<Option<Vec<DebounceWorker>>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    struct DebounceOverride(u64);

    impl DebounceOverride {
        fn set(ms: u64) -> Self {
            let previous = TEST_DEBOUNCE_MS.swap(ms, std::sync::atomic::Ordering::SeqCst);
            DEBOUNCE_WORKERS.with(|workers| *workers.borrow_mut() = Some(Vec::new()));
            Self(previous)
        }

        fn finish(&self) {
            let workers = DEBOUNCE_WORKERS
                .with(|workers| std::mem::take(workers.borrow_mut().as_mut().unwrap()));
            let handles: Vec<_> = workers
                .into_iter()
                .map(|(release, worker)| {
                    release.send(()).unwrap();
                    worker
                })
                .collect();
            for worker in handles {
                worker.join().expect("debounce worker panicked");
            }
        }
    }

    impl Drop for DebounceOverride {
        fn drop(&mut self) {
            let workers = DEBOUNCE_WORKERS.with(|workers| workers.borrow_mut().take().unwrap());
            for (release, worker) in workers {
                drop(release);
                let _ = worker.join();
            }
            set_test_debounce_ms(self.0);
        }
    }

    /// A stable transition fires once, a flicker back cancels, and a chain
    /// coalesces to the latest status against the original old status.
    #[test]
    #[serial]
    fn debounce_fires_only_the_settled_transition() {
        let config = StatusHookConfig {
            enabled: true,
            on_waiting: Some("notify-waiting".to_string()),
            on_idle: Some("notify-idle".to_string()),
            ..Default::default()
        };
        let cases: [(&[Status], Option<(&str, Status)>); 3] = [
            (
                &[Status::Running, Status::Waiting],
                Some(("notify-waiting", Status::Waiting)),
            ),
            (&[Status::Running, Status::Waiting, Status::Running], None),
            (
                &[Status::Running, Status::Waiting, Status::Idle],
                Some(("notify-idle", Status::Idle)),
            ),
        ];
        for (index, (chain, expected)) in cases.into_iter().enumerate() {
            reset_debounce_state();
            take_recorded_launches();
            let debounce = DebounceOverride::set(10);
            let mut instance = Instance::new("Debounce", "/tmp/project");
            instance.id = format!("debounce-{index}");

            let observed_before = Utc::now();
            for pair in chain.windows(2) {
                run_for_transition(&instance, pair[0], pair[1], &config);
            }
            let observed_after = Utc::now();
            assert!(take_recorded_launches().is_empty(), "{chain:?} fired early");

            debounce.finish();
            let launches = take_recorded_launches();
            match expected {
                None => assert!(launches.is_empty(), "{chain:?}"),
                Some((command, new_status)) => {
                    assert_eq!(launches.len(), 1, "{chain:?}");
                    assert_eq!(launches[0].command, command);
                    assert_eq!(launches[0].context.old_status, Status::Running);
                    assert_eq!(launches[0].context.new_status, new_status);
                    assert!(launches[0].context.changed_at >= observed_before);
                    assert!(launches[0].context.changed_at <= observed_after);
                }
            }
        }
    }

    #[test]
    fn commands_for_transition_runs_specific_then_catch_all() {
        let config = |on_waiting: &str| StatusHookConfig {
            enabled: true,
            on_waiting: Some(on_waiting.to_string()),
            on_change: Some("change-command".to_string()),
            ..Default::default()
        };
        let cases = [
            (StatusHookConfig::default(), Status::Running, &[][..]),
            (
                config("waiting-command"),
                Status::Running,
                &["waiting-command", "change-command"][..],
            ),
            // A blank command is skipped, and an unchanged status runs nothing.
            (config("  "), Status::Running, &["change-command"][..]),
            (config("waiting-command"), Status::Waiting, &[][..]),
        ];
        for (config, old, expected) in cases {
            assert_eq!(
                commands_for_transition(old, Status::Waiting, &config),
                expected,
                "{old:?} {:?}",
                config.on_waiting
            );
        }
    }

    /// `debounce_ms` was removed; configs that still carry it must deserialize.
    #[test]
    fn legacy_debounce_ms_is_ignored() {
        let config: StatusHookConfig = toml::from_str(
            r#"
            enabled = true
            debounce_ms = 500
            on_waiting = "notify-send waiting"
            "#,
        )
        .expect("legacy debounce_ms should not error");
        assert!(config.enabled);
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
