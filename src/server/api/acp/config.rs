//! Session mode and config-option selectors.

use serde::Deserialize;

use crate::acp::state::ConfigOptionCategory;

use super::*;

#[derive(Debug, Deserialize)]
pub struct SetModeRequest {
    pub mode_id: String,
}

#[derive(Debug, Deserialize)]
pub struct SetConfigOptionRequest {
    pub config_id: String,
    pub value: String,
}

/// Tally plan-mode adoption. `"plan"` is the plan value on both the mode and
/// the config-option channel, and no other category uses it.
fn count_plan_mode(state: &AppState, value: &str) {
    if value == "plan" {
        state
            .telemetry_structured
            .plan_mode_seen
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

pub async fn acp_set_mode(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    req: Result<Json<SetModeRequest>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if let Some(resp) = read_only_block(&state) {
        return resp;
    }
    if let Some(resp) = cityhall_block(&state) {
        return resp;
    }
    let Json(req) = match req {
        Ok(j) => j,
        Err(rej) => return rej.into_response(),
    };
    match state.acp_supervisor.set_mode(&id, &req.mode_id).await {
        Ok(()) => {
            count_plan_mode(&state, &req.mode_id);
            StatusCode::ACCEPTED.into_response()
        }
        Err(e) => supervisor_error_response("set_mode failed", &e),
    }
}

/// The persisted `Instance` field a config-option pick writes back to, so the
/// pick survives a worker respawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PersistedSelector {
    Model,
    Mode,
    ThoughtLevel,
}

impl PersistedSelector {
    fn field(self) -> &'static str {
        match self {
            Self::Model => "agent_model",
            Self::Mode => "acp_mode_id",
            Self::ThoughtLevel => "acp_effort",
        }
    }

    fn apply(self, inst: &mut crate::session::Instance, value: String) {
        match self {
            Self::Model => inst.agent_model = Some(value),
            Self::Mode => inst.acp_mode_id = Some(value),
            Self::ThoughtLevel => inst.acp_effort = Some(value),
        }
    }
}

/// Write a picked value into memory (read by the reconciler) and to disk.
async fn persist_selector(
    state: &Arc<AppState>,
    id: &str,
    selector: PersistedSelector,
    value: &str,
) {
    let profile = {
        let mut instances = state.instances.write().await;
        let Some(inst) = instances.iter_mut().find(|i| i.id == id) else {
            return;
        };
        selector.apply(inst, value.to_string());
        inst.source_profile.clone()
    };
    match crate::session::Storage::new(&profile, state.file_watch.clone()) {
        Ok(storage) => {
            if let Err(e) = storage.update(|instances, _groups| {
                if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
                    selector.apply(inst, value.to_string());
                }
                Ok(())
            }) {
                tracing::error!(
                    target: "http.api.acp",
                    session = %id,
                    field = selector.field(),
                    "failed to persist selector after config-option pick: {e}"
                );
            }
        }
        Err(e) => {
            tracing::error!(
                target: "http.api.acp",
                session = %id,
                field = selector.field(),
                "failed to open storage to persist selector after config-option pick: {e}"
            );
        }
    }
}

/// The category the session's agent advertised for `config_id`, from the
/// option catalog (the daemon keeps no live per-session option state).
async fn config_option_category(
    state: &Arc<AppState>,
    id: &str,
    config_id: &str,
) -> Option<ConfigOptionCategory> {
    let agent = {
        let instances = state.instances.read().await;
        instances.iter().find(|i| i.id == id).map(|i| {
            i.agent_name
                .as_deref()
                .filter(|s| !s.is_empty())
                .unwrap_or(i.tool.as_str())
                .to_string()
        })
    }?;
    crate::acp::option_catalog::load()
        .agents
        .get(&agent)
        .into_iter()
        .flat_map(|entry| entry.options.iter())
        .find(|opt| opt.id == config_id)
        .map(|opt| opt.category.clone())
}

/// Set a selector via ACP `session/set_config_option` (#1403). Rejection
/// surfaces as a `ConfigOptionSwitchFailed` event, not an HTTP error.
pub async fn acp_set_config_option(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    req: Result<Json<SetConfigOptionRequest>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if let Some(resp) = read_only_block(&state) {
        return resp;
    }
    if let Some(resp) = cityhall_block(&state) {
        return resp;
    }
    let Json(req) = match req {
        Ok(j) => j,
        Err(rej) => return rej.into_response(),
    };
    if let Err(e) = state
        .acp_supervisor
        .set_config_option(&id, &req.config_id, &req.value)
        .await
    {
        return supervisor_error_response("set_config_option failed", &e);
    }
    count_plan_mode(&state, &req.value);
    // The reconciler re-applies these fields on every spawn, so without the
    // write-back a respawn reverts the pick (#3086).
    let selector = if req.config_id == "model" {
        Some(PersistedSelector::Model)
    } else {
        match config_option_category(&state, &id, &req.config_id).await {
            Some(ConfigOptionCategory::Model) => Some(PersistedSelector::Model),
            Some(ConfigOptionCategory::Mode) => Some(PersistedSelector::Mode),
            Some(ConfigOptionCategory::ThoughtLevel) => Some(PersistedSelector::ThoughtLevel),
            Some(ConfigOptionCategory::Other(_)) | None => None,
        }
    };
    if let Some(selector) = selector {
        if selector == PersistedSelector::Model {
            state
                .acp_supervisor
                .refresh_cached_model(&id, &req.value)
                .await;
        }
        persist_selector(&state, &id, selector, &req.value).await;
    }
    StatusCode::ACCEPTED.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::state::{ConfigOptionChoice, ConfigOptionDescriptor};
    use crate::session::test_support::isolate_app_dir;

    #[tokio::test]
    #[serial_test::serial]
    async fn persist_selector_updates_memory_and_storage() {
        let cases = [
            (PersistedSelector::Model, "claude-sonnet-5"),
            (PersistedSelector::Mode, "agent-full-access"),
            (PersistedSelector::ThoughtLevel, "high"),
        ];
        for (selector, value) in cases {
            let _tmp = isolate_app_dir();
            let mut inst = crate::session::Instance::new("t", "/tmp");
            inst.source_profile = "default".to_string();
            inst.agent_model = Some("claude-sonnet-4-6".to_string());
            let id = inst.id.clone();
            let seed = inst.clone();
            crate::session::Storage::new_unwatched("default")
                .unwrap()
                .update(|instances, _groups| {
                    instances.push(seed);
                    Ok(())
                })
                .unwrap();

            let state = crate::server::test_support::build_test_app_state(vec![inst]);
            persist_selector(&state, &id, selector, value).await;

            let field = |inst: &crate::session::Instance| match selector {
                PersistedSelector::Model => inst.agent_model.clone(),
                PersistedSelector::Mode => inst.acp_mode_id.clone(),
                PersistedSelector::ThoughtLevel => inst.acp_effort.clone(),
            };
            assert_eq!(
                field(&state.instances.read().await[0]).as_deref(),
                Some(value)
            );
            let reloaded = crate::session::Storage::new_unwatched("default")
                .unwrap()
                .load()
                .unwrap();
            let on_disk = reloaded.iter().find(|i| i.id == id).unwrap();
            assert_eq!(field(on_disk).as_deref(), Some(value), "{selector:?}");
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn config_option_category_resolves_categories_from_catalog() {
        let _tmp = isolate_app_dir();
        let mut inst = crate::session::Instance::new("codex", "/tmp");
        inst.agent_name = Some("codex".to_string());
        let id = inst.id.clone();

        let option =
            |id: &str, category, choices: Vec<ConfigOptionChoice>| ConfigOptionDescriptor {
                id: id.to_string(),
                name: id.to_string(),
                description: None,
                category,
                current_value: String::new(),
                options: choices,
            };
        let choice = |value: &str| ConfigOptionChoice {
            value: value.to_string(),
            name: value.to_string(),
            description: None,
        };
        let opts = vec![
            option(
                "codex-mode",
                ConfigOptionCategory::Mode,
                vec![choice("agent-full-access")],
            ),
            option("model", ConfigOptionCategory::Model, vec![]),
            option(
                "reasoning-effort",
                ConfigOptionCategory::ThoughtLevel,
                vec![choice("high")],
            ),
        ];
        crate::acp::option_catalog::record("codex", &opts, "2026-07-24T00:00:00Z".to_string())
            .unwrap();
        let state = crate::server::test_support::build_test_app_state(vec![inst]);

        for (config_id, expected) in [
            ("codex-mode", Some(ConfigOptionCategory::Mode)),
            ("model", Some(ConfigOptionCategory::Model)),
            ("reasoning-effort", Some(ConfigOptionCategory::ThoughtLevel)),
            ("unknown", None),
        ] {
            assert_eq!(
                config_option_category(&state, &id, config_id).await,
                expected
            );
        }
    }
}
