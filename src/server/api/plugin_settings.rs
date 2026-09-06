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

use crate::session::config::settings_schema::{OptionSource, SelectOption};

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
        OptionSource::AcpModels => {
            // When the resolved profile pins the selected agent's model
            // (`acp.acp_defaults.<agent>.pin_model`), the picker collapses to
            // that one entry. Enforcement lives at creation (`sessions.create`
            // refuses any other model; the spawn resolver applies the pin on
            // every path), so this is presentation only: the wizard must not
            // offer choices the create call will refuse. The catalog supplies
            // the display label when it knows the pinned model.
            if let Some(agent) = depends.first().filter(|a| !a.is_empty()) {
                if let Some(model) = pinned_model_for_agent(&state.profile, agent).await {
                    let label = catalog_options(Some(agent), CatalogCategory::Model)
                        .into_iter()
                        .find(|opt| opt.value == model)
                        .map(|opt| opt.label)
                        .unwrap_or_else(|| model.clone());
                    return Ok(vec![SelectOption::new(&model, &label)]);
                }
            }
            Ok(catalog_options_probing(depends.first(), CatalogCategory::Model).await)
        }
        OptionSource::AcpModes => {
            Ok(catalog_options_probing(depends.first(), CatalogCategory::Mode).await)
        }
        OptionSource::Projects => project_options(&state.profile).await,
        OptionSource::Groups => Ok(group_options(state).await),
    }
}

/// Registry agents whose ACP adapter is filtered by `present`, mapped to
/// `{value,label}`. Split out so the install filter is unit-testable without
/// depending on which adapters happen to be on the test host's PATH.
fn installed_agent_options<'a>(
    entries: impl IntoIterator<Item = (&'a String, &'a crate::acp::AgentSpec)>,
    present: impl Fn(&str) -> bool,
) -> Vec<SelectOption> {
    entries
        .into_iter()
        .filter(|(_, spec)| present(&spec.command))
        .map(|(name, _)| SelectOption::new(name, name))
        .collect()
}

/// ACP-capable agents from the static registry whose adapter binary actually
/// resolves on this host, plus any custom ACP agents the resolved profile
/// config declares via a valid `agent_acp_cmd`. Sorted, deduped by id (a custom
/// entry shadowing a built-in is dropped by the dedup).
///
/// The registry filter mirrors `list_agents` (`acp_installed`): an agent is
/// only offered as a choice when the host could actually launch it, so the
/// picker never lists uninstalled harnesses (#3-plugin-cron picker fix).
async fn acp_agent_options(profile: &str) -> Vec<SelectOption> {
    let registry = crate::acp::AgentRegistry::with_defaults();
    let mut opts = installed_agent_options(registry.list(), crate::cli::acp::command_present);

    // Custom ACP agents live in the per-profile config; resolve the profile
    // (global -> profile, no repo) and keep entries whose command parses as a
    // valid ACP adapter. Config IO runs off the async runtime.
    let profile = profile.to_string();
    let custom = tokio::task::spawn_blocking(move || {
        let session =
            crate::session::config::profile_config::resolve_config_or_warn(&profile).session;
        let detect_as = &session.agent_detect_as;
        session
            .agent_acp_cmd
            .iter()
            .filter(|(name, cmd)| {
                !name.is_empty() && crate::acp::AgentSpec::from_acp_cmd(name, cmd).is_ok()
            })
            .map(|(name, _)| name.clone())
            // Plus custom agents that inherit a registry-backed base via
            // `agent_detect_as` (e.g. a Claude wrapper); they run in structured
            // view through the base adapter.
            .chain(
                detect_as
                    .keys()
                    .filter(|name| {
                        !name.is_empty()
                            && crate::acp::inherited_acp_base(name, detect_as).is_some()
                    })
                    .cloned(),
            )
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

#[derive(Clone, Copy)]
enum CatalogCategory {
    Model,
    Mode,
}

/// Like [`catalog_options`], but when the selected agent's catalog has never
/// been discovered, run a one-shot handshake probe to populate it first, so a
/// model/mode picker self-fills on first open instead of showing empty until
/// the agent has run a live session. The probe records into the shared option
/// catalog, so it is effectively one spawn per agent; the cache then suppresses
/// repeats.
async fn catalog_options_probing(
    agent: Option<&String>,
    category: CatalogCategory,
) -> Vec<SelectOption> {
    let first = catalog_options(agent, category);
    if !first.is_empty() {
        return first;
    }
    let Some(agent) = agent.filter(|a| !a.is_empty()) else {
        return first;
    };
    // Already discovered (this category is just genuinely empty): don't respawn.
    if crate::acp::option_catalog::load()
        .agents
        .contains_key(agent)
    {
        return first;
    }
    // Only registry agents are blind-probed; a custom agent's command can carry
    // secrets, so it stays populated only by real runs.
    if crate::acp::AgentRegistry::with_defaults()
        .get(agent)
        .is_none()
    {
        return first;
    }
    // ponytail: one handshake spawn per undiscovered agent; an agent that
    // advertises no options at all re-probes on each open (rare, cheap).
    match crate::acp::capability_probe::probe_agent(agent).await {
        Ok(true) => catalog_options(Some(agent), category),
        _ => first,
    }
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

/// The model pinned for `agent` in the resolved profile config, if any
/// (`acp.acp_defaults.<agent>.pin_model = true` with a `model`). A plain
/// `model` without the flag is a default, not a pin, and yields `None`: an
/// explicit request still wins over a default at creation, so the picker must
/// keep offering the full catalog for it.
async fn pinned_model_for_agent(profile: &str, agent: &str) -> Option<String> {
    let profile = profile.to_string();
    let agent = agent.to_string();
    tokio::task::spawn_blocking(move || {
        crate::session::config::profile_config::resolve_config_or_warn(&profile)
            .acp
            .pinned_model_for(&agent)
    })
    .await
    .ok()
    .flatten()
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

        // Agents: filtered to adapters present on this host, so the exact set
        // is environment-dependent; just assert the resolver succeeds and only
        // ever returns known registry ids (no custom agents in this profile).
        let agents = resolve_option_source(&state, OptionSource::AcpAgents, &[])
            .await
            .expect("agents");
        let registry = crate::acp::AgentRegistry::with_defaults();
        assert!(agents.iter().all(|o| registry.get(&o.value).is_some()));

        // Models with no selected agent: empty (nothing to resolve yet).
        let models = resolve_option_source(&state, OptionSource::AcpModels, &[])
            .await
            .expect("models");
        assert!(models.is_empty());
        // Models for a registry-unknown agent: empty and hermetic. A *known*
        // undiscovered agent would trigger a live handshake probe (see
        // `catalog_options_probing`), which is not something a unit test should
        // spawn, so we assert the empty path via an id the probe declines.
        let models = resolve_option_source(
            &state,
            OptionSource::AcpModels,
            &["definitely-not-an-agent-xyz".to_string()],
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

    #[test]
    fn agent_filter_keeps_only_present_adapters() {
        let registry = crate::acp::AgentRegistry::with_defaults();
        let total = registry.list().len();
        assert!(total > 0, "registry should have default agents");

        // No adapter present -> empty picker (the uninstalled-harness fix).
        assert!(installed_agent_options(registry.list(), |_| false).is_empty());

        // All present -> every registry entry, one option each.
        assert_eq!(
            installed_agent_options(registry.list(), |_| true).len(),
            total
        );

        // A predicate that matches a single command keeps only that agent.
        let (name, spec) = registry.list().into_iter().next().expect("one agent");
        let want_cmd = spec.command.clone();
        let picked = installed_agent_options(registry.list(), |cmd| cmd == want_cmd);
        assert!(picked.iter().any(|o| &o.value == name));
    }

    /// The picker collapses only on an explicit pin. A plain
    /// `acp_defaults.<agent>.model` is a default (an explicit request still
    /// wins at `sessions.create`), so it must keep the full catalog.
    #[tokio::test]
    #[serial_test::serial]
    async fn picker_collapses_on_a_pin_but_not_on_a_default() {
        use crate::session::test_support::isolate_app_dir;
        let _tmp = isolate_app_dir();
        let config_path = crate::session::get_app_dir()
            .expect("isolated app dir")
            .join("config.toml");
        std::fs::write(
            &config_path,
            "[acp.acp_defaults.opencode]\nmodel = \"openai/gpt-5.5\"\n\n\
             [acp.acp_defaults.claude]\nmodel = \"claude-pinned\"\npin_model = true\n",
        )
        .expect("write config");

        assert_eq!(pinned_model_for_agent("default", "opencode").await, None);
        assert_eq!(
            pinned_model_for_agent("default", "claude").await.as_deref(),
            Some("claude-pinned")
        );
        assert_eq!(pinned_model_for_agent("default", "gemini").await, None);
    }
}
