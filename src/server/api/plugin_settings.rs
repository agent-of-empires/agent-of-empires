//! Host option-source resolver for plugin `dynamic_select` widgets (#2897).
//!
//! A `dynamic_select` names an [`OptionSource`]; the host resolves the actual
//! choices from its own state (agent registry, ACP option catalog, project
//! registry, session groups). The web and TUI renderers stay ignorant of
//! where a source's data comes from: they post the source plus any
//! `depends_on` values and render the returned `{value,label}` list. Saved
//! ids are authoritatively revalidated at `sessions.create`, so this endpoint
//! is advisory UI data, not an authorization surface; it still requires an
//! authenticated dashboard session like every other `/api/*` route.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::session::settings_schema::{OptionSource, SelectOption};

use super::super::AppState;

#[derive(Debug, Deserialize)]
pub struct ResolveOptionsRequest {
    /// The option source, in the same snake_case form the widget schema
    /// serializes (`acp_agents`, `acp_models`, ...).
    pub source: OptionSource,
    /// Values of the `depends_on` sibling fields, in declaration order. For
    /// `acp.models` / `acp.modes` the first entry is the selected agent id.
    #[serde(default)]
    pub depends: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ResolveOptionsResponse {
    pub options: Vec<SelectOption>,
}

/// `POST /api/plugins/{id}/settings/options/resolve`: resolve one
/// dynamic-select source for the settings UI. The `{id}` path segment scopes
/// the request to a plugin for auditing/consistency but does not change the
/// result: option sources are host-global.
pub async fn resolve_options(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(_plugin_id): axum::extract::Path<String>,
    req: Result<Json<ResolveOptionsRequest>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    let Json(req) = match req {
        Ok(j) => j,
        Err(rej) => return rej.into_response(),
    };
    match resolve_option_source(&state, req.source, &req.depends).await {
        Ok(options) => Json(ResolveOptionsResponse { options }).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("option resolve failed: {e:#}"),
        )
            .into_response(),
    }
}

/// Resolve a dynamic-select option source to a normalized `{value,label}`
/// list. Shared by the HTTP endpoint (web) and any in-process caller (TUI).
pub async fn resolve_option_source(
    state: &Arc<AppState>,
    source: OptionSource,
    depends: &[String],
) -> anyhow::Result<Vec<SelectOption>> {
    match source {
        OptionSource::AcpAgents => Ok(acp_agent_options(&state.profile).await),
        OptionSource::AcpModels => Ok(catalog_options(depends.first(), CatalogCategory::Model)),
        OptionSource::AcpModes => Ok(catalog_options(depends.first(), CatalogCategory::Mode)),
        OptionSource::Projects => project_options(&state.profile).await,
        OptionSource::Groups => Ok(group_options(state).await),
    }
}

/// ACP-capable agents from the static registry plus any custom ACP agents the
/// resolved profile config declares via a valid `agent_acp_cmd`. Sorted, deduped
/// by id (a custom entry shadowing a built-in is dropped by the dedup).
async fn acp_agent_options(profile: &str) -> Vec<SelectOption> {
    let registry = crate::acp::AgentRegistry::with_defaults();
    let mut opts: Vec<SelectOption> = registry
        .list()
        .into_iter()
        .map(|(name, _)| SelectOption::new(name, name))
        .collect();

    // Custom ACP agents live in the per-profile config; resolve the profile
    // (global -> profile, no repo) and keep entries whose command parses as a
    // valid ACP adapter. Config IO runs off the async runtime.
    let profile = profile.to_string();
    let custom = tokio::task::spawn_blocking(move || {
        crate::session::profile_config::resolve_config_or_warn(&profile)
            .session
            .agent_acp_cmd
            .into_iter()
            .filter(|(name, cmd)| {
                !name.is_empty() && crate::acp::AgentSpec::from_acp_cmd(name, cmd).is_ok()
            })
            .map(|(name, _)| name)
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();
    for name in custom {
        opts.push(SelectOption::new(&name, &name));
    }

    opts.sort_by(|a, b| a.value.cmp(&b.value));
    opts.dedup_by(|a, b| a.value == b.value);
    opts
}

enum CatalogCategory {
    Model,
    Mode,
}

/// Model or mode choices the given agent last advertised. Empty when no agent
/// is selected yet or the agent's catalog has not been discovered; the UI then
/// shows an empty/"run the agent first" state, and sessions.create is the
/// authoritative validator regardless.
fn catalog_options(agent: Option<&String>, category: CatalogCategory) -> Vec<SelectOption> {
    let Some(agent) = agent.filter(|a| !a.is_empty()) else {
        return Vec::new();
    };
    let catalog = crate::acp::option_catalog::load();
    let Some(entry) = catalog.agents.get(agent) else {
        return Vec::new();
    };
    let want = match category {
        CatalogCategory::Model => crate::acp::state::ConfigOptionCategory::Model,
        CatalogCategory::Mode => crate::acp::state::ConfigOptionCategory::Mode,
    };
    entry
        .options
        .iter()
        .filter(|opt| opt.category == want)
        .flat_map(|opt| opt.options.iter())
        .map(|choice| SelectOption::new(&choice.value, &choice.name))
        .collect()
}

async fn project_options(profile: &str) -> anyhow::Result<Vec<SelectOption>> {
    let profile = profile.to_string();
    let projects =
        tokio::task::spawn_blocking(move || crate::session::projects::load_merged(&profile))
            .await??;
    Ok(projects
        .into_iter()
        .map(|p| SelectOption::new(&p.path, &p.name))
        .collect())
}

async fn group_options(state: &Arc<AppState>) -> Vec<SelectOption> {
    let instances = state.instances.read().await;
    let mut paths: Vec<String> = instances
        .iter()
        .filter(|i| !i.group_path.is_empty())
        .map(|i| i.group_path.clone())
        .collect();
    paths.sort();
    paths.dedup();
    paths.iter().map(|p| SelectOption::new(p, p)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Instance;

    #[tokio::test]
    async fn resolves_agents_models_and_groups() {
        let mut a = Instance::new("one", "/tmp/p");
        a.group_path = "work/backend".to_string();
        let mut b = Instance::new("two", "/tmp/q");
        b.group_path = "work/backend".to_string();
        let state = crate::server::test_support::build_test_app_state(vec![a, b]);

        // Agents: the static ACP registry always has entries.
        let agents = resolve_option_source(&state, OptionSource::AcpAgents, &[])
            .await
            .expect("agents");
        assert!(agents.iter().any(|o| o.value == "claude-code"));

        // Models with no selected agent: empty (nothing to resolve yet).
        let models = resolve_option_source(&state, OptionSource::AcpModels, &[])
            .await
            .expect("models");
        assert!(models.is_empty());
        // Models for an agent with no discovered catalog: still empty, never errors.
        let models = resolve_option_source(
            &state,
            OptionSource::AcpModels,
            &["claude-code".to_string()],
        )
        .await
        .expect("models");
        assert!(models.is_empty());

        // Groups: derived from live instances, deduped.
        let groups = resolve_option_source(&state, OptionSource::Groups, &[])
            .await
            .expect("groups");
        assert_eq!(
            groups.iter().map(|o| o.value.as_str()).collect::<Vec<_>>(),
            vec!["work/backend"]
        );
    }
}
