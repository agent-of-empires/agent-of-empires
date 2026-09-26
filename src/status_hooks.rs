//! Local command hooks for session status transitions.

use std::collections::HashMap;
#[cfg(not(test))]
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aoe_settings_derive::SettingsSection;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[cfg(test)]
use crate::session::Instance;
use crate::session::Status;

/// Milliseconds a status must remain stable before hook commands run, so
/// rapid flickers (Running -> Waiting -> Running) don't fire spurious hooks.
#[cfg(not(test))]
const DEFAULT_DEBOUNCE_MS: u64 = 100;

/// Test-only override selecting synchronous dispatch or gated debounce workers.
/// Mutating tests share the serial group with the recorded-launches buffer.
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

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, SettingsSection)]
#[setting_section(name = "status_hooks", category = "Status Hooks")]
pub struct StatusHookConfig {
    /// Run local commands when the core observes session status changes.
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
    #[cfg(test)]
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

#[derive(Default)]
pub(crate) struct StatusHooks {
    state: Arc<Mutex<DebounceState>>,
}

#[derive(Default)]
struct DebounceState {
    entries: HashMap<String, DebounceEntry>,
    configs: HashMap<String, StatusHookConfig>,
    generation: u64,
}

impl StatusHooks {
    pub(crate) fn reconcile(
        &self,
        rows: &[crate::daemon::SessionResponse],
        configs: &HashMap<String, StatusHookConfig>,
    ) {
        let mut state = self.state.lock().unwrap();
        if !state.entries.is_empty() {
            let profiles: HashMap<_, _> = rows
                .iter()
                .map(|row| (row.id.as_str(), row.profile.as_str()))
                .collect();
            let DebounceState {
                entries,
                configs: previous,
                ..
            } = &mut *state;
            entries.retain(|id, entry| {
                let Some(&profile) = profiles.get(id.as_str()) else {
                    return false;
                };
                profile == entry.profile
                    && configs.get(profile).is_some_and(|config| {
                        config.enabled && previous.get(profile) == Some(config)
                    })
            });
        }
        if state.configs != *configs {
            state.configs.clone_from(configs);
        }
    }

    pub(crate) fn prepare_transition(
        &self,
        mut context: StatusHookContext,
        config: &StatusHookConfig,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> Option<impl std::future::Future<Output = ()> + Send + 'static> {
        let old = context.old_status;
        let new = context.new_status;
        if !config.enabled || old == new || shutdown.is_cancelled() {
            return None;
        }
        let commands = commands_for_transition(old, new, config);
        let debounce_ms = effective_debounce_ms();
        let generation = if debounce_ms > 0 {
            let mut state = self.state.lock().unwrap();
            state.generation = state
                .generation
                .checked_add(1)
                .expect("status hook generation exhausted");
            let generation = state.generation;
            let entry = state
                .entries
                .entry(context.session_id.clone())
                .or_insert(DebounceEntry {
                    profile: context.profile.clone(),
                    stable_status: old,
                    generation: 0,
                    pending_status: None,
                });
            entry.generation = generation;
            if new == entry.stable_status {
                entry.pending_status = None;
                return None;
            }
            if commands.is_empty() {
                entry.stable_status = new;
                entry.pending_status = None;
                return None;
            }
            context.old_status = entry.stable_status;
            entry.pending_status = Some(new);
            Some(entry.generation)
        } else {
            if commands.is_empty() {
                return None;
            }
            None
        };
        let state = self.state.clone();
        Some(async move {
            if let Some(generation) = generation {
                tokio::select! {
                    _ = shutdown.cancelled() => return,
                    _ = tokio::time::sleep(Duration::from_millis(debounce_ms)) => {}
                }
                let mut state = state.lock().unwrap();
                match state.entries.get_mut(&context.session_id) {
                    Some(entry)
                        if entry.generation == generation && entry.pending_status == Some(new) =>
                    {
                        entry.stable_status = new;
                        entry.pending_status = None;
                    }
                    _ => return,
                }
            }
            if shutdown.is_cancelled() {
                return;
            }
            #[cfg(not(test))]
        if let Err(error) = tokio::task::spawn_blocking(move || {
            let project_path = PathBuf::from(&context.project_path);
            for command in commands {
                if shutdown.is_cancelled() {
                    break;
                }
                if let Err(error) = run_hook_command_blocking(&command, &context, &project_path, &shutdown) {
                    if !shutdown.is_cancelled() {
                        tracing::warn!(target: "hooks.status_hooks", session_id = %context.session_id, %error, "status hook failed");
                    }
                }
            }
        }).await {
            tracing::error!(target: "hooks.status_hooks", %error, "status hook worker failed");
        }
            #[cfg(test)]
            {
                let mut launches = recorded_launches().lock().unwrap();
                for command in commands {
                    launches.push(RecordedLaunch {
                        command,
                        context: context.clone(),
                    });
                }
            }
        })
    }
}

fn non_empty_command(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|s| !s.is_empty())
}

#[derive(Debug, Clone)]
struct DebounceEntry {
    profile: String,
    stable_status: Status,
    generation: u64,
    pending_status: Option<Status>,
}

/// Upper bound for one status-hook command.
#[cfg(not(test))]
const HOOK_COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

#[cfg(not(test))]
fn run_hook_command_blocking(
    command: &str,
    context: &StatusHookContext,
    project_path: &Path,
    shutdown: &tokio_util::sync::CancellationToken,
) -> std::io::Result<()> {
    match crate::process::run_status_with_timeout_process_group(
        &mut build_command(command, context, project_path),
        HOOK_COMMAND_TIMEOUT,
        || shutdown.is_cancelled(),
    )? {
        Some(status) if status.success() => Ok(()),
        Some(status) => Err(std::io::Error::other(format!(
            "command exited with status {:?}",
            status.code()
        ))),
        None => Err(std::io::Error::other(format!(
            "command timed out after {}s",
            HOOK_COMMAND_TIMEOUT.as_secs()
        ))),
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
mod tests {
    use super::*;
    use serial_test::serial;
    /// RAII guard restoring the test debounce override to 0 (the synchronous
    /// path other tests rely on) even when an assertion panics.
    struct DebounceOverride;

    impl DebounceOverride {
        fn set(ms: u64) -> Self {
            set_test_debounce_ms(ms);
            Self
        }
    }

    impl Drop for DebounceOverride {
        fn drop(&mut self) {
            set_test_debounce_ms(0);
        }
    }

    #[tokio::test]
    #[serial]
    async fn reenabling_hooks_observes_transitions_after_disabled_interval() {
        let _guard = crate::session::test_support::isolate_app_dir();
        let _debounce = DebounceOverride::set(10);
        take_recorded_launches();
        let mut row = Instance::new("hook reactivation", "/repo");
        row.source_profile = "test".into();
        row.status = Status::Idle;
        let state = crate::server::test_support::build_test_app_state(vec![row]);
        state.canonical_metadata.write().await.status_hooks.insert(
            "test".into(),
            StatusHookConfig {
                enabled: true,
                on_running: Some("running-notification".into()),
                ..Default::default()
            },
        );
        state.runtime.publish(&state).await.unwrap();
        state.instances.write().await[0].status = Status::Running;
        state.runtime.publish(&state).await.unwrap();
        state.runtime.work.drain().await;
        assert_eq!(take_recorded_launches().len(), 1);
        state
            .canonical_metadata
            .write()
            .await
            .status_hooks
            .get_mut("test")
            .unwrap()
            .enabled = false;
        state.runtime.publish(&state).await.unwrap();
        state.instances.write().await[0].status = Status::Idle;
        state.runtime.publish(&state).await.unwrap();
        state
            .canonical_metadata
            .write()
            .await
            .status_hooks
            .get_mut("test")
            .unwrap()
            .enabled = true;
        state.runtime.publish(&state).await.unwrap();
        state.instances.write().await[0].status = Status::Running;
        state.runtime.publish(&state).await.unwrap();
        state.runtime.work.drain().await;
        let launches = take_recorded_launches();
        assert_eq!(
            launches.len(),
            1,
            "reactivation lost a real Idle-to-Running transition"
        );
        assert_eq!(launches[0].context.old_status, Status::Idle);
        assert_eq!(launches[0].context.new_status, Status::Running);
    }

    #[tokio::test]
    #[serial]
    async fn pending_hooks_follow_profile_ownership_without_reusing_tickets() {
        let _guard = crate::session::test_support::isolate_app_dir();
        let _debounce = DebounceOverride::set(10);
        take_recorded_launches();
        let mut instance = Instance::new("pending hook", "/repo");
        instance.source_profile = "test".into();
        let state = crate::server::test_support::build_test_app_state(vec![instance.clone()]);
        let snapshot = state.runtime.publish(&state).await.unwrap();
        let original = &snapshot.value.contents.sessions;
        let config = StatusHookConfig {
            enabled: true,
            on_running: Some("notify-running".into()),
            ..Default::default()
        };
        let configs = HashMap::from([
            ("test".into(), config.clone()),
            ("other".into(), config.clone()),
        ]);
        for change in ["disabled", "removed", "moved", "reconfigured", "recreated"] {
            let hooks = StatusHooks::default();
            hooks.reconcile(original, &configs);
            let context = StatusHookContext::from_instance(
                &instance,
                Status::Idle,
                Status::Running,
                Utc::now(),
            );
            let pending = hooks
                .prepare_transition(context.clone(), &config, Default::default())
                .unwrap();
            let mut rows = original.clone();
            let mut changed = configs.clone();
            match change {
                "disabled" => changed.get_mut("test").unwrap().enabled = false,
                "removed" | "recreated" => rows.clear(),
                "moved" => rows[0].profile = "other".into(),
                "reconfigured" => {
                    changed.get_mut("test").unwrap().on_running = Some("new-command".into())
                }
                _ => unreachable!(),
            }
            hooks.reconcile(&rows, &changed);
            if change == "recreated" {
                hooks.reconcile(original, &configs);
                let replacement = hooks
                    .prepare_transition(context, &config, Default::default())
                    .unwrap();
                tokio::join!(pending, replacement);
                assert_eq!(
                    take_recorded_launches().len(),
                    1,
                    "an obsolete ticket matched a recreated row"
                );
            } else {
                pending.await;
                assert!(
                    take_recorded_launches().is_empty(),
                    "pending hook survived {change}"
                );
                hooks.reconcile(original, &configs);
                hooks
                    .prepare_transition(context, &config, Default::default())
                    .unwrap()
                    .await;
                assert_eq!(
                    take_recorded_launches().len(),
                    1,
                    "restored ownership lost a transition after {change}"
                );
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

    #[tokio::test]
    #[serial]
    async fn debounces_stable_transition() {
        let hooks = StatusHooks::default();
        take_recorded_launches();
        let _debounce = DebounceOverride::set(10);

        let mut instance = Instance::new("Debounce Stable", "/tmp/project");
        instance.id = "debounce-stable".to_string();
        let config = StatusHookConfig {
            enabled: true,
            on_waiting: Some("notify-waiting".to_string()),
            ..Default::default()
        };

        let observed_before = Utc::now();
        let task = hooks
            .prepare_transition(
                StatusHookContext::from_instance(
                    &instance,
                    Status::Running,
                    Status::Waiting,
                    observed_before,
                ),
                &config,
                Default::default(),
            )
            .unwrap();
        let observed_after = Utc::now();
        assert!(take_recorded_launches().is_empty());

        task.await;
        let launches = take_recorded_launches();
        assert_eq!(launches.len(), 1);
        assert_eq!(launches[0].command, "notify-waiting");
        assert_eq!(launches[0].context.old_status, Status::Running);
        assert_eq!(launches[0].context.new_status, Status::Waiting);
        assert!(launches[0].context.changed_at >= observed_before);
        assert!(launches[0].context.changed_at <= observed_after);
    }

    #[tokio::test]
    #[serial]
    async fn debounce_cancels_flicker_back_to_stable_status() {
        let hooks = StatusHooks::default();
        take_recorded_launches();
        let _debounce = DebounceOverride::set(10);

        let mut instance = Instance::new("Debounce Flicker", "/tmp/project");
        instance.id = "debounce-flicker".to_string();
        let config = StatusHookConfig {
            enabled: true,
            on_waiting: Some("notify-waiting".to_string()),
            ..Default::default()
        };

        let pending = hooks
            .prepare_transition(
                StatusHookContext::from_instance(
                    &instance,
                    Status::Running,
                    Status::Waiting,
                    Utc::now(),
                ),
                &config,
                Default::default(),
            )
            .unwrap();
        assert!(hooks
            .prepare_transition(
                StatusHookContext::from_instance(
                    &instance,
                    Status::Waiting,
                    Status::Running,
                    Utc::now()
                ),
                &config,
                Default::default()
            )
            .is_none());
        pending.await;
        assert!(take_recorded_launches().is_empty());
    }

    #[tokio::test]
    #[serial]
    async fn debounce_coalesces_to_latest_pending_status() {
        let hooks = StatusHooks::default();
        take_recorded_launches();
        let _debounce = DebounceOverride::set(10);

        let mut instance = Instance::new("Debounce Latest", "/tmp/project");
        instance.id = "debounce-latest".to_string();
        let config = StatusHookConfig {
            enabled: true,
            on_waiting: Some("notify-waiting".to_string()),
            on_idle: Some("notify-idle".to_string()),
            ..Default::default()
        };

        let first = hooks
            .prepare_transition(
                StatusHookContext::from_instance(
                    &instance,
                    Status::Running,
                    Status::Waiting,
                    Utc::now(),
                ),
                &config,
                Default::default(),
            )
            .unwrap();
        let last = hooks
            .prepare_transition(
                StatusHookContext::from_instance(
                    &instance,
                    Status::Waiting,
                    Status::Idle,
                    Utc::now(),
                ),
                &config,
                Default::default(),
            )
            .unwrap();
        tokio::join!(first, last);
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
