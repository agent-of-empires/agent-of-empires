//! Web setup wizard state. Step progress is derived from real configuration on
//! every read; `state.toml` only records which steps were skipped and whether
//! the wizard was finished, deferred, or its dashboard banner dismissed.

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

/// Keys in the global `config.toml` that mark a step as configured. Typed
/// config can't tell an explicit value from a default, so presence is read raw.
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
        &[
            "enabled_by_default",
            "container_runtime",
            "allow_image_pull",
        ],
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
                inputs
                    .config
                    .get(*section)
                    .and_then(toml::Value::as_table)
                    .is_some_and(|table| keys.iter().any(|k| table.contains_key(*k)))
            }),
    }
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SetupStep {
    pub key: String,
    pub required: bool,
    pub configured: bool,
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
    STEPS
        .iter()
        .map(|(key, required)| SetupStep {
            key: key.to_string(),
            required: *required,
            configured: step_configured(key, inputs),
            skipped: skipped.contains(key),
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

/// Validate a client-sent skip set and return it in catalog order.
fn canonical_skips(requested: &[String]) -> Result<Vec<String>, String> {
    for key in requested {
        match STEPS.iter().find(|(step, _)| step == key) {
            None => return Err(format!("unknown setup step '{key}'")),
            Some((_, true)) => {
                return Err(format!(
                    "setup step '{key}' is required and cannot be skipped"
                ))
            }
            Some(_) => {}
        }
    }
    Ok(known_steps(requested)
        .into_iter()
        .map(str::to_string)
        .collect())
}

fn load_inputs(project_exists: bool) -> SetupInputs {
    let config = crate::session::Config::load_or_warn();
    let raw = crate::session::config::config_path()
        .ok()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|content| content.parse::<toml::Table>().ok())
        .unwrap_or_default();
    let agents = crate::tmux::AvailableTools::detect_with_custom_agents(&config.session);
    SetupInputs {
        telemetry_answered: config.app_state.has_responded_to_telemetry
            || crate::telemetry::do_not_track(),
        agent_installed: !agents.available_list().is_empty(),
        project_exists,
        config: raw,
    }
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
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<SetupStatus> {
        let app = AppStateConfig::load()?;
        let inputs = load_inputs(projects);
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
    skipped_steps: Option<Vec<String>>,
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
    let skipped = match patch
        .skipped_steps
        .as_deref()
        .map(canonical_skips)
        .transpose()
    {
        Ok(skipped) => skipped,
        Err(message) => return api_error(StatusCode::BAD_REQUEST, "validation_failed", message),
    };
    let projects = project_exists(&state).await;
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<Result<(), String>> {
        if patch.finished == Some(true) {
            let inputs = load_inputs(projects);
            if let Some((key, _)) = STEPS
                .iter()
                .find(|(key, required)| *required && !step_configured(key, &inputs))
            {
                return Ok(Err(format!("setup step '{key}' is required")));
            }
        }
        crate::session::update_app_state(|app| {
            if let Some(skipped) = skipped {
                app.setup_skipped_steps = skipped;
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
            ..Default::default()
        }
    }

    #[test]
    fn config_steps_are_configured_only_by_recognized_keys() {
        for (config, step, expected) in [
            ("", "worktrees", false),
            ("[worktree]\n", "worktrees", false),
            (
                "[worktree]\nbare_repo_path_template = \"x\"\n",
                "worktrees",
                false,
            ),
            ("[worktree]\nenabled = false\n", "worktrees", true),
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
    fn skips_are_validated_and_canonical() {
        let strings = |keys: &[&str]| keys.iter().map(|k| k.to_string()).collect::<Vec<_>>();
        assert_eq!(
            canonical_skips(&strings(&["sandbox", "project", "sandbox"])),
            Ok(strings(&["project", "sandbox"]))
        );
        for bad in ["agents", "Sandbox", "", "bogus"] {
            assert!(canonical_skips(&strings(&[bad])).is_err(), "{bad}");
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
            "skipped_steps": ["sandbox"],
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
            send(serde_json::json!({ "skipped_steps": ["agents"] })).await,
            StatusCode::BAD_REQUEST
        );
    }
}
