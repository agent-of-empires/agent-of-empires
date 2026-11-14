//! Web setup wizard state. Step progress is derived from real configuration on
//! every read; `state.toml` only records which steps were skipped or confirmed
//! and whether the wizard was finished, deferred, or its banner dismissed.

use std::sync::Arc;

use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use serde::{Deserialize, Serialize};

use super::{api_error, AppState};
use crate::session::config::AppStateConfig;

/// The closed step catalog, in wizard order: `(key, required)`.
const STEPS: [(&str, bool); 6] = [
    ("theme_telemetry", false),
    ("agents", true),
    ("project", false),
    ("worktrees", false),
    ("sandbox", false),
    ("notifications", false),
];

/// Global `config.toml` keys that configure a step. Every config write saves
/// the whole file, so a step counts as configured only when one of these
/// differs from its default; keeping a default is recorded by confirming it.
const CONFIG_KEYS: [(&str, &str, &[&str]); 3] = [
    (
        "worktrees",
        "worktree",
        &[
            "enabled",
            "path_template",
            "default_base_branch",
            "auto_cleanup",
        ],
    ),
    (
        "sandbox",
        "sandbox",
        &["enabled_by_default", "container_runtime", "default_image"],
    ),
    (
        "notifications",
        "web",
        &[
            "notifications_enabled",
            "notify_on_waiting",
            "notify_on_idle",
            "notify_on_error",
        ],
    ),
];

#[derive(Debug, Default)]
struct SetupInputs {
    telemetry_answered: bool,
    agent_installed: bool,
    project_exists: bool,
    config: toml::Table,
    defaults: toml::Table,
}

fn step_configured(key: &str, inputs: &SetupInputs) -> bool {
    match key {
        "theme_telemetry" => inputs.telemetry_answered,
        "agents" => inputs.agent_installed,
        "project" => inputs.project_exists,
        _ => CONFIG_KEYS
            .iter()
            .find(|(step, _, _)| *step == key)
            .is_some_and(|(_, section, keys)| {
                let table =
                    |t: &toml::Table| t.get(*section).and_then(toml::Value::as_table).cloned();
                let (set, defaults) = (table(&inputs.config), table(&inputs.defaults));
                set.is_some_and(|set| {
                    keys.iter().any(|k| {
                        set.get(*k)
                            .is_some_and(|v| defaults.as_ref().and_then(|d| d.get(*k)) != Some(v))
                    })
                })
            }),
    }
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SetupStep {
    pub key: String,
    pub required: bool,
    /// Set up, from config or by keeping the defaults in the wizard.
    pub configured: bool,
    /// The user kept this optional step's defaults in the wizard.
    pub confirmed: bool,
    pub skipped: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SetupAccess {
    pub serve_mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tunnel_url: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SetupStatus {
    pub finished: bool,
    pub deferred: bool,
    pub banner_dismissed: bool,
    pub steps: Vec<SetupStep>,
    pub tmux: bool,
    pub git: bool,
    pub access: SetupAccess,
}

fn build_steps(inputs: &SetupInputs, app: &AppStateConfig) -> Vec<SetupStep> {
    let skipped = known_steps(&app.setup_skipped_steps);
    let confirmed = known_steps(&app.setup_confirmed_steps);
    STEPS
        .iter()
        .map(|(key, required)| {
            // A required step is only ever done by being configured.
            let confirmed = !required && confirmed.contains(key);
            SetupStep {
                key: key.to_string(),
                required: *required,
                configured: confirmed || step_configured(key, inputs),
                confirmed,
                skipped: skipped.contains(key),
            }
        })
        .collect()
}

/// Stored skip keys a newer or older build no longer knows are dropped.
fn known_steps(stored: &[String]) -> Vec<&'static str> {
    STEPS
        .iter()
        .filter(|(key, _)| stored.iter().any(|s| s == key))
        .map(|(key, _)| *key)
        .collect()
}

/// Validate a client-sent optional step key.
fn optional_step(key: &str) -> Result<String, String> {
    match STEPS.iter().find(|(step, _)| *step == key) {
        None => Err(format!("unknown setup step '{key}'")),
        Some((_, true)) => Err(format!("setup step '{key}' is required")),
        Some(_) => Ok(key.to_string()),
    }
}

/// The first required step that isn't set up, which blocks finishing.
fn open_required_step(inputs: &SetupInputs) -> Option<&'static str> {
    STEPS
        .iter()
        .find(|(key, required)| *required && !step_configured(key, inputs))
        .map(|(key, _)| *key)
}

fn load_inputs(profile: &str, project_exists: bool) -> SetupInputs {
    // Typed, so a value `Config` rejects counts as unset rather than configured.
    let config = crate::session::Config::load_or_warn();
    SetupInputs {
        telemetry_answered: config.app_state.has_responded_to_telemetry
            || crate::telemetry::do_not_track(),
        agent_installed: !detect_agents(profile).available_list().is_empty(),
        project_exists,
        config: toml::Table::try_from(&config).unwrap_or_default(),
        defaults: toml::from_str::<crate::session::Config>("")
            .ok()
            .and_then(|c| toml::Table::try_from(c).ok())
            .unwrap_or_default(),
    }
}

/// The Agents step lists the served Profile's Agents, so detect the same set.
fn detect_agents(profile: &str) -> crate::tmux::AvailableTools {
    crate::tmux::AvailableTools::detect_with_custom_agents(
        &crate::session::config::profile_config::resolve_config_or_warn(profile).session,
    )
}

async fn project_exists(state: &AppState) -> bool {
    let metadata = state.canonical_metadata.read().await;
    !metadata.global_projects.is_empty() || metadata.profiles.iter().any(|p| !p.projects.is_empty())
}

/// The public tunnel URL from `serve.url`, without its token.
fn tunnel_url(serve_mode: &str) -> Option<String> {
    if serve_mode == "local" {
        return None;
    }
    let content =
        std::fs::read_to_string(crate::session::get_app_dir().ok()?.join("serve.url")).ok()?;
    let first = content.lines().next()?.trim();
    let base = first.split('?').next()?.trim_end_matches('/');
    (!base.is_empty()).then(|| base.to_string())
}

fn internal_error(e: impl std::fmt::Display) -> axum::response::Response {
    tracing::error!("setup task failed: {e}");
    api_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal",
        "Couldn't read setup state.",
    )
}

pub async fn get_setup(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let projects = project_exists(&state).await;
    let serve_mode = state.serve_mode;
    let profile = state.served_profile().clone();
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<SetupStatus> {
        let app = AppStateConfig::load()?;
        let inputs = load_inputs(&profile, projects);
        Ok(SetupStatus {
            finished: app.setup_finished,
            deferred: app.setup_deferred,
            banner_dismissed: app.setup_banner_dismissed,
            steps: build_steps(&inputs, &app),
            tmux: crate::tmux::is_tmux_available(),
            git: which::which("git").is_ok(),
            access: SetupAccess {
                serve_mode: serve_mode.to_string(),
                tunnel_url: tunnel_url(serve_mode),
            },
        })
    })
    .await;
    match result {
        Ok(Ok(status)) => (StatusCode::OK, Json(status)).into_response(),
        Ok(Err(e)) => internal_error(e),
        Err(e) => internal_error(e),
    }
}

#[derive(Deserialize)]
pub struct SetupPatch {
    skip_step: Option<String>,
    confirm_step: Option<String>,
    unskip_step: Option<String>,
    finished: Option<bool>,
    deferred: Option<bool>,
    banner_dismissed: Option<bool>,
}

pub async fn patch_setup(
    State(state): State<Arc<AppState>>,
    body: Result<Json<SetupPatch>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return super::read_only_response();
    }
    if let Some(resp) = super::cityhall_block(&state) {
        return resp;
    }
    let Json(patch) = match body {
        Ok(b) => b,
        Err(e) => {
            return api_error(StatusCode::BAD_REQUEST, "validation_failed", e.body_text());
        }
    };
    // Per-step deltas, so concurrent writers compose inside `update_app_state`.
    let validate = |key: &Option<String>| key.as_deref().map(optional_step).transpose();
    let (skip, confirm, unskip) = match (
        validate(&patch.skip_step),
        validate(&patch.confirm_step),
        validate(&patch.unskip_step),
    ) {
        (Ok(skip), Ok(confirm), Ok(unskip)) => (skip, confirm, unskip),
        (Err(message), _, _) | (_, Err(message), _) | (_, _, Err(message)) => {
            return api_error(StatusCode::BAD_REQUEST, "validation_failed", message)
        }
    };
    let projects = project_exists(&state).await;
    let profile = state.served_profile().clone();
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<Result<(), String>> {
        if patch.finished == Some(true) {
            if let Some(key) = open_required_step(&load_inputs(&profile, projects)) {
                return Ok(Err(format!("setup step '{key}' is required")));
            }
        }
        crate::session::update_app_state(|app| {
            if let Some(key) = unskip {
                app.setup_skipped_steps.retain(|k| *k != key);
            }
            if let Some(key) = skip {
                app.setup_confirmed_steps.retain(|k| *k != key);
                if !app.setup_skipped_steps.contains(&key) {
                    app.setup_skipped_steps.push(key);
                }
            }
            if let Some(key) = confirm {
                app.setup_skipped_steps.retain(|k| *k != key);
                if !app.setup_confirmed_steps.contains(&key) {
                    app.setup_confirmed_steps.push(key);
                }
            }
            // Finishing is one-way; revisiting /setup doesn't reopen the redirect.
            app.setup_finished |= patch.finished.unwrap_or(false);
            if let Some(deferred) = patch.deferred {
                app.setup_deferred = deferred;
            }
            if let Some(dismissed) = patch.banner_dismissed {
                app.setup_banner_dismissed = dismissed;
            }
        })?;
        Ok(Ok(()))
    })
    .await;
    match result {
        Ok(Ok(Ok(()))) => StatusCode::NO_CONTENT.into_response(),
        Ok(Ok(Err(message))) => api_error(StatusCode::BAD_REQUEST, "required_step_open", message),
        Ok(Err(e)) => internal_error(e),
        Err(e) => internal_error(e),
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RuntimeStatus {
    pub kind: crate::session::config::ContainerRuntimeName,
    pub installed: bool,
    pub running: bool,
    /// How to start it on the Daemon's platform.
    pub start_command: &'static str,
}

fn start_command(kind: crate::session::config::ContainerRuntimeName, macos: bool) -> &'static str {
    use crate::session::config::ContainerRuntimeName as Name;
    match (kind, macos) {
        (Name::Docker, true) => "docker desktop start",
        (Name::Docker, false) => "sudo systemctl start docker",
        (Name::Podman, true) => "podman machine start",
        (Name::Podman, false) => "systemctl --user start podman.socket",
        (Name::AppleContainer, _) => "container system start",
    }
}

/// Every container runtime this platform supports, probed separately so the
/// Sandbox step can say which one to start. `docker info` can be slow, so this
/// is its own request rather than part of `GET /api/setup`.
pub async fn get_setup_runtimes() -> impl IntoResponse {
    use crate::session::config::ContainerRuntimeName as Name;
    let result = tokio::task::spawn_blocking(|| {
        let mut kinds = vec![Name::Docker, Name::Podman];
        if cfg!(target_os = "macos") {
            kinds.push(Name::AppleContainer);
        }
        kinds
            .into_iter()
            .map(|kind| {
                let runtime = crate::containers::ContainerRuntime::from(kind);
                let installed = runtime.is_available();
                RuntimeStatus {
                    kind,
                    installed,
                    running: installed && runtime.is_daemon_running(),
                    start_command: start_command(kind, cfg!(target_os = "macos")),
                }
            })
            .collect::<Vec<_>>()
    })
    .await;
    match result {
        Ok(runtimes) => (StatusCode::OK, Json(runtimes)).into_response(),
        Err(e) => internal_error(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs(config: &str) -> SetupInputs {
        SetupInputs {
            config: config.parse().unwrap(),
            defaults: toml::Table::try_from(toml::from_str::<crate::session::Config>("").unwrap())
                .unwrap(),
            ..Default::default()
        }
    }

    #[test]
    fn config_steps_are_configured_only_by_non_default_values() {
        // Every config write saves every key, so a full default file configures nothing.
        let full = toml::to_string(&inputs("").defaults).unwrap();
        for (config, step, expected) in [
            ("", "worktrees", false),
            (full.as_str(), "worktrees", false),
            (full.as_str(), "sandbox", false),
            (full.as_str(), "notifications", false),
            ("[worktree]\nenabled = false\n", "worktrees", false),
            ("[worktree]\nenabled = true\n", "worktrees", true),
            (
                "[worktree]\nbare_repo_path_template = \"x\"\n",
                "worktrees",
                false,
            ),
            (
                "[worktree]\ndefault_base_branch = \"develop\"\n",
                "worktrees",
                true,
            ),
            (
                "[sandbox]\ncontainer_runtime = \"podman\"\n",
                "sandbox",
                true,
            ),
            ("[web]\nmcp_enabled = true\n", "notifications", false),
            ("[web]\nnotify_on_idle = true\n", "notifications", true),
        ] {
            assert_eq!(
                step_configured(step, &inputs(config)),
                expected,
                "{step} with {config:?}"
            );
        }
    }

    #[test]
    fn confirming_configures_only_optional_steps() {
        let app = AppStateConfig {
            setup_confirmed_steps: vec!["sandbox".into(), "agents".into()],
            ..Default::default()
        };
        let configured: Vec<_> = build_steps(&inputs(""), &app)
            .into_iter()
            .filter(|s| s.configured)
            .map(|s| s.key)
            .collect();
        assert_eq!(configured, ["sandbox"]);
    }

    #[test]
    fn step_keys_are_validated() {
        assert_eq!(optional_step("sandbox"), Ok("sandbox".to_string()));
        for bad in ["agents", "Sandbox", "", "bogus"] {
            assert!(optional_step(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn stored_skips_drop_retired_steps() {
        let app = AppStateConfig {
            setup_skipped_steps: vec!["retired".into(), "sandbox".into()],
            ..Default::default()
        };
        let skipped: Vec<_> = build_steps(&SetupInputs::default(), &app)
            .into_iter()
            .filter(|s| s.skipped)
            .map(|s| s.key)
            .collect();
        assert_eq!(skipped, ["sandbox"]);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn patch_persists_and_finish_is_one_way() {
        let _home = crate::session::test_support::isolate_app_dir();
        let state = crate::server::test_support::build_test_app_state(Vec::new());
        let send = |body: serde_json::Value| {
            let state = state.clone();
            async move {
                patch_setup(
                    State(state),
                    Ok(Json(serde_json::from_value(body).unwrap())),
                )
                .await
                .into_response()
                .status()
            }
        };
        let status = send(serde_json::json!({
            "skip_step": "sandbox",
            "deferred": true,
            "banner_dismissed": true,
        }))
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let app = AppStateConfig::load().unwrap();
        assert_eq!(app.setup_skipped_steps, ["sandbox"]);
        assert!(app.setup_deferred && app.setup_banner_dismissed && !app.setup_finished);

        crate::session::update_app_state(|app| app.setup_finished = true).unwrap();
        assert_eq!(
            send(serde_json::json!({ "finished": false })).await,
            StatusCode::NO_CONTENT
        );
        assert!(AppStateConfig::load().unwrap().setup_finished);

        assert_eq!(
            send(serde_json::json!({ "skip_step": "agents" })).await,
            StatusCode::BAD_REQUEST
        );

        assert_eq!(
            send(serde_json::json!({ "confirm_step": "sandbox" })).await,
            StatusCode::NO_CONTENT
        );
        let app = AppStateConfig::load().unwrap();
        assert_eq!(app.setup_confirmed_steps, ["sandbox"]);
        assert!(app.setup_skipped_steps.is_empty());

        assert_eq!(
            send(serde_json::json!({ "skip_step": "sandbox" })).await,
            StatusCode::NO_CONTENT
        );
        let app = AppStateConfig::load().unwrap();
        assert_eq!(app.setup_skipped_steps, ["sandbox"]);
        assert!(app.setup_confirmed_steps.is_empty());

        // Deltas compose: skipping another step keeps the first skip.
        for body in [
            serde_json::json!({ "skip_step": "worktrees" }),
            serde_json::json!({ "unskip_step": "sandbox" }),
        ] {
            assert_eq!(send(body).await, StatusCode::NO_CONTENT);
        }
        assert_eq!(
            AppStateConfig::load().unwrap().setup_skipped_steps,
            ["worktrees"]
        );
    }

    #[test]
    #[serial_test::serial]
    fn agents_come_from_the_served_profile() {
        let _home = crate::session::test_support::isolate_app_dir();
        crate::session::config::profile_config::update_profile_config("work", |config| {
            config.overrides.insert(
                "session".into(),
                serde_json::json!({ "custom_agents": { "mine": "sh" } }),
            );
            Ok(())
        })
        .unwrap();
        assert!(detect_agents("work")
            .available_list()
            .contains(&"mine".to_string()));
    }

    #[test]
    fn start_commands_match_the_daemon_platform() {
        use crate::session::config::ContainerRuntimeName as Name;
        assert_eq!(
            start_command(Name::Docker, false),
            "sudo systemctl start docker"
        );
        assert_eq!(start_command(Name::Docker, true), "docker desktop start");
        assert_eq!(
            start_command(Name::Podman, false),
            "systemctl --user start podman.socket"
        );
    }

    #[test]
    fn finishing_needs_an_installed_agent() {
        let mut inputs = SetupInputs::default();
        assert_eq!(open_required_step(&inputs), Some("agents"));
        inputs.agent_installed = true;
        assert_eq!(open_required_step(&inputs), None);
    }
}
