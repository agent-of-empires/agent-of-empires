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
    let Some(_submission) = state
        .session_service
        .prompt_submission_for_session(&id)
        .await
    else {
        return session_not_found();
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

/// Write a picked value to disk, then to memory (read by the reconciler). Memory
/// is untouched on failure so it keeps matching the stored row.
async fn persist_selector(
    state: &Arc<AppState>,
    id: &str,
    selector: PersistedSelector,
    value: &str,
) -> anyhow::Result<()> {
    let _reload = state.session_service.disk_mutation_guard().await;
    let profile = {
        let instances = state.instances.read().await;
        let Some(inst) = instances.iter().find(|i| i.id == id) else {
            return Ok(());
        };
        inst.source_profile.clone()
    };
    crate::session::Storage::new(&profile, state.file_watch.clone())?.update(
        |instances, _groups| {
            if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
                selector.apply(inst, value.to_string());
            }
            Ok(())
        },
    )?;
    let mut instances = state.instances.write().await;
    if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
        selector.apply(inst, value.to_string());
        state
            .mutation_epoch
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
    Ok(())
}

/// The key the session's agent is recorded under in the option catalog.
async fn catalog_agent(state: &Arc<AppState>, id: &str) -> Option<String> {
    let instances = state.instances.read().await;
    instances.iter().find(|i| i.id == id).map(|i| {
        i.agent_name
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(i.tool.as_str())
            .to_string()
    })
}

/// The category the session's agent advertised for `config_id`, from the
/// option catalog (the daemon keeps no live per-session option state).
async fn config_option_category(
    state: &Arc<AppState>,
    id: &str,
    config_id: &str,
) -> Option<ConfigOptionCategory> {
    let agent = catalog_agent(state, id).await?;
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
    let Some(_submission) = state
        .session_service
        .prompt_submission_for_session(&id)
        .await
    else {
        return session_not_found();
    };
    // Respawns reuse persisted selector picks.
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
    // The adapter rejects a bad effort only after the pick is stored, so an
    // unknown value must not get that far. No cached choices means accept.
    if selector == Some(PersistedSelector::ThoughtLevel) {
        let agent = catalog_agent(&state, &id).await.unwrap_or_default();
        let choices = crate::acp::option_catalog::thought_level_choices(&agent);
        if !choices.is_empty() && !choices.contains(&req.value) {
            return super::super::api_error(
                StatusCode::BAD_REQUEST,
                "invalid_effort",
                format!(
                    "unknown effort {:?}; expected one of: {}",
                    req.value,
                    choices.join(", ")
                ),
            );
        }
    }
    if let Err(e) = state
        .acp_supervisor
        .set_config_option(&id, &req.config_id, &req.value)
        .await
    {
        return supervisor_error_response("set_config_option failed", &e);
    }
    if let Some(selector) = selector {
        if let Err(e) = persist_selector(&state, &id, selector, &req.value).await {
            return persist_failure_response(
                &state,
                &id,
                &format!("config-option pick for {}", selector.field()),
                &e,
            )
            .await;
        }
        if selector == PersistedSelector::Model {
            state
                .acp_supervisor
                .refresh_cached_model(&id, &req.value)
                .await;
        }
    }
    count_plan_mode(&state, &req.value);
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

            let stale = inst.clone();
            let state = crate::server::test_support::build_test_app_state(vec![inst]);
            let read_epoch = state
                .mutation_epoch
                .load(std::sync::atomic::Ordering::SeqCst);
            let metadata = state.canonical_metadata.read().await.clone();
            persist_selector(&state, &id, selector, value)
                .await
                .unwrap();
            crate::server::reload::reload_state_instances_from_disk(
                &state,
                (vec![stale]).into(),
                vec![],
                crate::server::state::StatusSource::DiskOnly,
                read_epoch,
                Ok(metadata),
                std::collections::HashMap::new(),
            )
            .await;

            let field = |inst: &crate::session::Instance| match selector {
                PersistedSelector::Model => inst.agent_model.clone(),
                PersistedSelector::Mode => inst.acp_mode_id.clone(),
                PersistedSelector::ThoughtLevel => inst.acp_effort.clone(),
            };
            assert_eq!(
                field(&state.instances.read().await[0]).as_deref(),
                Some(value),
                "a stale reload must not replace the selector"
            );
            let reloaded = crate::session::Storage::new_unwatched("default")
                .unwrap()
                .load()
                .unwrap();
            let on_disk = reloaded.iter().find(|i| i.id == id).unwrap();
            assert_eq!(field(on_disk).as_deref(), Some(value), "{selector:?}");
            let storage = crate::session::Storage::new_unwatched("default").unwrap();
            storage
                .update(|rows, _| {
                    rows.iter_mut().find(|row| row.id == id).unwrap().title =
                        "accepted fresh reload".into();
                    Ok(())
                })
                .unwrap();
            let epoch = state
                .mutation_epoch
                .load(std::sync::atomic::Ordering::SeqCst);
            let (fresh, fresh_metadata) =
                crate::server::reload::load_profiles_for_reload(&state.file_watch);
            crate::server::reload::reload_state_instances_from_disk(
                &state,
                fresh,
                vec![],
                crate::server::state::StatusSource::DiskOnly,
                epoch,
                fresh_metadata,
                Default::default(),
            )
            .await;
            let live = state.instances.read().await;
            assert_eq!(live[0].title, "accepted fresh reload");
            assert_eq!(field(&live[0]).as_deref(), Some(value));
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn a_failed_selector_write_leaves_memory_unchanged() {
        for selector in [
            PersistedSelector::Model,
            PersistedSelector::Mode,
            PersistedSelector::ThoughtLevel,
        ] {
            let _tmp = isolate_app_dir();
            let mut inst = crate::session::Instance::new("t", "/tmp");
            inst.source_profile = "blocked".to_string();
            inst.agent_model = Some("old-model".to_string());
            inst.acp_mode_id = Some("old-mode".to_string());
            inst.acp_effort = Some("old-effort".to_string());
            let id = inst.id.clone();
            crate::server::test_support::block_profile_storage_for_test("blocked");
            let state = crate::server::test_support::build_test_app_state(vec![inst]);
            let before = state
                .mutation_epoch
                .load(std::sync::atomic::Ordering::SeqCst);

            persist_selector(&state, &id, selector, "new")
                .await
                .unwrap_err();

            let memory = state.instances.read().await;
            assert_eq!(memory[0].agent_model.as_deref(), Some("old-model"));
            assert_eq!(memory[0].acp_mode_id.as_deref(), Some("old-mode"));
            assert_eq!(memory[0].acp_effort.as_deref(), Some("old-effort"));
            assert_eq!(
                state
                    .mutation_epoch
                    .load(std::sync::atomic::Ordering::SeqCst),
                before,
                "{selector:?}"
            );
        }
    }

    /// The worker takes the pick, the stored write fails: the caller gets the
    /// 500, the worker is stopped, and neither the plan-mode tally nor memory
    /// records the pick.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_failed_pick_write_stops_the_worker_and_reports_it() {
        let _tmp = isolate_app_dir();
        let mut inst = crate::session::Instance::new("t", "/tmp");
        inst.source_profile = "blocked".to_string();
        inst.agent_model = Some("old-model".to_string());
        let id = inst.id.clone();
        crate::server::test_support::block_profile_storage_for_test("blocked");
        let state = crate::server::test_support::build_test_app_state(vec![inst]);
        let cmds = state
            .acp_supervisor
            .test_insert_worker_cmd_recording(&id)
            .await;

        let resp = acp_set_config_option(
            State(state.clone()),
            Path(id.clone()),
            Ok(Json(SetConfigOptionRequest {
                config_id: "model".to_string(),
                value: "plan".to_string(),
            })),
        )
        .await
        .into_response();

        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&body).contains("persist_failed"));
        assert!(
            !cmds.lock().unwrap().is_empty(),
            "the worker took the pick before the write failed"
        );
        assert_eq!(
            state.acp_supervisor.worker_state(&id).await,
            crate::daemon::AcpWorkerState::Absent
        );
        assert_eq!(
            state
                .telemetry_structured
                .plan_mode_seen
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        assert_eq!(
            state.instances.read().await[0].agent_model.as_deref(),
            Some("old-model")
        );
    }

    /// The write fails on a valid stored row (not an unopenable profile): the
    /// 500 leaves both stores on the old pick, and the retry after recovery
    /// lands in both.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_pick_write_failing_on_a_valid_row_recovers_on_retry() {
        use crate::server::test_support as support;
        let _tmp = isolate_app_dir();
        let mut inst = crate::session::Instance::new("t", "/tmp");
        inst.source_profile = "default".to_string();
        inst.agent_model = Some("old-model".to_string());
        let id = inst.id.clone();
        support::seed_instances_on_disk_for_test("default", vec![inst.clone()]);
        let state = support::build_test_app_state(vec![inst]);
        let pick = || async {
            acp_set_config_option(
                State(state.clone()),
                Path(id.clone()),
                Ok(Json(SetConfigOptionRequest {
                    config_id: "model".to_string(),
                    value: "new-model".to_string(),
                })),
            )
            .await
            .into_response()
        };

        state
            .acp_supervisor
            .test_insert_worker_cmd_recording(&id)
            .await;
        let lock = support::lock_profile_dir_for_test("default");
        if !lock.blocks_writes() {
            return;
        }
        assert_eq!(pick().await.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            state.instances.read().await[0].agent_model.as_deref(),
            Some("old-model")
        );
        assert_eq!(
            support::load_instances_from_disk_for_test("default")[0]
                .agent_model
                .as_deref(),
            Some("old-model")
        );

        drop(lock);
        state
            .acp_supervisor
            .test_insert_worker_cmd_recording(&id)
            .await;
        assert_eq!(pick().await.status(), StatusCode::ACCEPTED);
        assert_eq!(
            state.instances.read().await[0].agent_model.as_deref(),
            Some("new-model")
        );
        assert_eq!(
            support::load_instances_from_disk_for_test("default")[0]
                .agent_model
                .as_deref(),
            Some("new-model")
        );
    }

    /// An unknown effort is refused before the worker or storage see it. A
    /// known one, or any value with no cached choices, reaches the worker
    /// (absent here, hence 404).
    #[tokio::test]
    #[serial_test::serial]
    async fn unknown_effort_is_rejected_before_it_is_sent_or_stored() {
        let _tmp = isolate_app_dir();
        let option = |category, values: &[&str]| ConfigOptionDescriptor {
            id: "effort".to_string(),
            name: "effort".to_string(),
            description: None,
            category,
            current_value: String::new(),
            options: values
                .iter()
                .map(|v| ConfigOptionChoice {
                    value: v.to_string(),
                    name: v.to_string(),
                    description: None,
                })
                .collect(),
        };
        crate::acp::option_catalog::record(
            "codex",
            &[option(ConfigOptionCategory::ThoughtLevel, &["low", "high"])],
            "2026-07-24T00:00:00Z".to_string(),
        )
        .unwrap();
        crate::acp::option_catalog::record(
            "claude",
            &[option(ConfigOptionCategory::ThoughtLevel, &[])],
            "2026-07-24T00:00:00Z".to_string(),
        )
        .unwrap();

        for (agent, value, expected) in [
            ("codex", "hgih", StatusCode::BAD_REQUEST),
            ("codex", "high", StatusCode::NOT_FOUND),
            ("claude", "anything", StatusCode::NOT_FOUND),
        ] {
            let mut inst = crate::session::Instance::new(agent, "/tmp");
            inst.agent_name = Some(agent.to_string());
            let id = inst.id.clone();
            let state = crate::server::test_support::build_test_app_state(vec![inst]);
            let resp = acp_set_config_option(
                State(state.clone()),
                Path(id),
                Ok(Json(SetConfigOptionRequest {
                    config_id: "effort".to_string(),
                    value: value.to_string(),
                })),
            )
            .await
            .into_response();
            assert_eq!(resp.status(), expected, "{agent} {value}");
            assert_eq!(state.instances.read().await[0].acp_effort, None);
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
