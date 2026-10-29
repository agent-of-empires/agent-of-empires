//! Misc system endpoints: agents, settings, themes, profiles, filesystem,
//! groups, docker status, system health, devices, about.

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    extract::{Extension, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};

use super::api_error;
use super::validate_profile_name;
use super::AppState;
use crate::server::auth::AuthenticatedTokenHash;
use crate::server::auth::{handler_elevated, AuthenticatedSession};
use crate::server::principal::{authorize, Capability, Principal};
use crate::session::config::settings_schema::{
    clear_path, rewrite_plugin_sections, runtime_schema, strip_local_only, validate_patch,
    validate_patch_with, PatchRejection, Scope,
};

/// Foreground state reported by one browser dashboard. This is intentionally
/// separate from normal API traffic: background polling must not suppress a
/// phone's push notification.
#[derive(Deserialize)]
pub struct DashboardPresenceBody {
    pub active: bool,
}

/// `POST /api/presence`. Record or clear this browser's foreground presence.
/// The device-binding header is already attached to authenticated dashboard
/// requests. Hashing it gives each browser an ephemeral server-side key without
/// retaining the secret itself. Older clients without that header fall back to
/// their authenticated token owner.
pub async fn post_dashboard_presence(
    State(state): State<Arc<AppState>>,
    owner: Option<Extension<AuthenticatedTokenHash>>,
    headers: HeaderMap,
    Json(body): Json<DashboardPresenceBody>,
) -> StatusCode {
    let Some(Extension(owner)) = owner else {
        return StatusCode::NO_CONTENT;
    };
    let client = headers
        .get("x-aoe-device-binding")
        .and_then(|value| value.to_str().ok())
        .map(crate::server::push::sha256_token)
        .unwrap_or(owner.0);
    state.set_web_presence(client, body.active);
    StatusCode::NO_CONTENT
}

// --- Agents ---

#[derive(Serialize)]
pub struct AgentInfo {
    pub kind: String,
    pub name: String,
    /// How copy names the agent; custom agents are named by `name`.
    pub display_name: String,
    pub binary: String,
    pub host_only: bool,
    pub installed: bool,
    pub install_hint: String,
    /// True when this agent has a one-shot mode (a `oneshot_flag`), so it can
    /// be used for the smart-rename title call. The settings smart-rename agent
    /// picker filters on this together with `installed`. Always false for
    /// custom agents (no built-in one-shot contract).
    pub oneshot_capable: bool,
    /// True when this agent can run in the structured acp UI: a
    /// built-in with an ACP adapter, or a custom agent that declares a
    /// valid `agent_acp_cmd`. The web wizard reads this to decide
    /// whether a session created for the agent runs in acp or tmux.
    pub acp_capable: bool,
    /// Whether a non-sandboxed structured create in the served profile would find this agent's
    /// ACP adapter. With no project, relative PATH entries and the profile's own hooks count;
    /// the repository's hooks are left to the create.
    pub acp_installed: bool,
    /// True when `[acp] allowed_agents` permits this agent in the structured
    /// view. Deliberately separate from `acp_capable`, which states an intrinsic
    /// fact (an ACP adapter exists for this agent) that an operator policy does
    /// not change. Folding policy into `acp_capable` would hide a disallowed
    /// agent from the settings surfaces that enumerate this endpoint to edit
    /// per-agent structured-view defaults, which is a legitimate thing to do for
    /// an agent that is currently off the allowlist. The wizard gates its
    /// structured-view option on this in addition to `acp_capable`. See #3241.
    pub acp_allowed: bool,
    /// The ACP command a built-in agent launches in acp, e.g.
    /// `claude-agent-acp` for claude or `opencode` for opencode. This is
    /// the registry command (post `${aoe_data_dir}` substitution), which
    /// can differ from `binary`; the wizard previews it so the user sees
    /// the real launch command before starting. Omitted for custom
    /// agents, whose command values are never serialized here (see the
    /// custom-agent serialization tests below).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acp_command: Option<String>,
    /// The registry args appended to `acp_command` (e.g. `["acp"]`
    /// for opencode, `["--acp"]` for gemini). Empty when there are none
    /// or for custom agents.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub acp_args: Vec<String>,
    /// Registry lifecycle state. Omitted while Active so the common wire
    /// shape is unchanged; the dashboard mirrors the shape in
    /// `web/src/lib/types.ts` (`AgentLifecycleInfo`) and renders a
    /// deprecated badge in the wizard picker and switch-agent modal.
    #[serde(skip_serializing_if = "crate::agents::AgentLifecycle::is_active")]
    pub lifecycle: crate::agents::AgentLifecycle,
}

/// Resolve the acp launch command + args for a built-in agent from
/// its registry spec, substituting `${aoe_data_dir}` so the preview
/// matches what `supervisor::spawn_inner` actually runs. Returns
/// `(None, [])` for agents without a registry entry.
fn acp_command_fields(
    spec: Option<&crate::acp::AgentSpec>,
    data_dir: Option<&std::path::Path>,
) -> (Option<String>, Vec<String>) {
    let substitute = |value: &str| match data_dir {
        Some(dir) if value.contains("${aoe_data_dir}") => {
            value.replace("${aoe_data_dir}", &dir.to_string_lossy())
        }
        _ => value.to_string(),
    };
    match spec {
        Some(spec) => {
            let command = substitute(&spec.command);
            let args = spec.args.iter().map(|arg| substitute(arg)).collect();
            (Some(command), args)
        }
        None => (None, Vec::new()),
    }
}

fn build_custom_agent_infos(
    custom_agents: &HashMap<String, String>,
    agent_acp_cmd: &HashMap<String, String>,
    agent_detect_as: &HashMap<String, String>,
    policy: &crate::acp::agent_policy::AgentPolicy,
) -> Vec<AgentInfo> {
    let mut entries: Vec<_> = custom_agents
        .iter()
        .filter(|(name, command)| {
            !name.trim().is_empty()
                && !command.trim().is_empty()
                && crate::agents::get_agent(name).is_none()
        })
        .map(|(name, _command)| AgentInfo {
            lifecycle: crate::agents::AgentLifecycle::Active,
            kind: "custom".to_string(),
            name: name.clone(),
            display_name: name.clone(),
            binary: name.clone(),
            host_only: false,
            installed: true,
            install_hint: "Configured custom agent".to_string(),
            oneshot_capable: false,
            acp_capable: agent_acp_cmd
                .get(name)
                .is_some_and(|cmd| crate::acp::AgentSpec::from_acp_cmd(name, cmd).is_ok())
                || crate::acp::inherited_acp_base(name, agent_detect_as).is_some(),
            acp_allowed: policy.allows(name),
            // Set by `list_agents`, which holds the config the probe needs.
            acp_installed: false,
            // Custom agents' command values are deliberately never
            // serialized here; they can hold hostnames or secrets.
            acp_command: None,
            acp_args: Vec::new(),
        })
        .collect();
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    entries
}

pub async fn list_agents(State(state): State<Arc<AppState>>) -> Json<Vec<AgentInfo>> {
    let _namespace = state.profile_namespace.read().await;
    let profile = state.served_profile().to_string();
    let result = tokio::task::spawn_blocking(move || {
        let config = crate::session::config::profile_config::resolve_config_or_warn(&profile);
        let tools = crate::tmux::AvailableTools::detect();
        let available = tools.available_list();
        let acp_registry = crate::acp::AgentRegistry::with_defaults();
        // Global config, not `config` above: the allowlist is an operator
        // control and a profile override must not widen it (#3241).
        let policy = crate::acp::agent_policy::AgentPolicy::load();
        let data_dir = crate::session::get_app_dir().ok();
        let mut agents = crate::agents::AGENTS
            .iter()
            .map(|a| {
                let (acp_command, acp_args) =
                    acp_command_fields(acp_registry.get(a.name), data_dir.as_deref());
                AgentInfo {
                    kind: "builtin".to_string(),
                    name: a.name.to_string(),
                    display_name: crate::acp::display_name_for(a.name)
                        .unwrap_or(a.name)
                        .to_string(),
                    binary: a.binary.to_string(),
                    host_only: a.host_only,
                    installed: available.iter().any(|s| s == a.name),
                    install_hint: a.install_hint.to_string(),
                    oneshot_capable: a.oneshot_flag.is_some(),
                    lifecycle: a.lifecycle,
                    acp_capable: acp_registry.get(a.name).is_some(),
                    acp_installed: false,
                    acp_allowed: policy.allows(a.name),
                    acp_command,
                    acp_args,
                }
            })
            .collect::<Vec<_>>();
        agents.extend(build_custom_agent_infos(
            &config.session.custom_agents,
            &config.session.agent_acp_cmd,
            &config.session.agent_detect_as,
            &policy,
        ));
        let host_environment =
            crate::session::environment::resolve_host_environment_pairs(&config.environment);
        for agent in agents.iter_mut().filter(|a| a.acp_capable) {
            agent.acp_installed = crate::session::builder::structured::adapter_installed(
                &agent.name,
                &config,
                &host_environment,
            );
        }
        agents
    })
    .await
    .unwrap_or_else(|e| {
        tracing::error!("list_agents task failed: {e}");
        Vec::new()
    });
    Json(result)
}

// --- Settings ---

#[derive(Deserialize)]
pub struct SettingsQuery {
    /// The profile settings are resolved over machine-wide. Defaults to the
    /// served profile, the one `/api/about` names.
    pub profile: Option<String>,
    /// `profile` names the profile layer explicitly, `machine` addresses the
    /// machine-wide layer alone. Any other value is refused.
    pub layer: Option<String>,
}

/// The store a settings read or write addresses.
enum SettingsLayer {
    /// A profile resolved over machine-wide settings.
    Profile(String),
    /// Machine-wide `config.toml` alone.
    Machine,
}

impl SettingsLayer {
    /// Resolve the layer a request names. The `Err` message is the 400 body.
    fn resolve(query: &SettingsQuery, served: &str) -> Result<Self, String> {
        match query.layer.as_deref() {
            Some("machine") => {
                if query.profile.is_some() {
                    return Err(
                        "`profile` cannot pick a profile when `layer=machine` addresses the \
                         machine-wide layer"
                            .to_string(),
                    );
                }
                Ok(Self::Machine)
            }
            Some("profile") => Ok(Self::Profile(requested_profile(query, served)?)),
            Some(other) => Err(format!(
                "Unknown settings layer '{other}'; expected `profile` or `machine`"
            )),
            None => Ok(Self::Profile(requested_profile(query, served)?)),
        }
    }
}

/// The profile a request names, validated before it reaches the filesystem.
fn requested_profile(query: &SettingsQuery, served: &str) -> Result<String, String> {
    match &query.profile {
        Some(name) => {
            validate_profile_name(name)?;
            Ok(name.clone())
        }
        None => Ok(served.to_string()),
    }
}

fn bad_request(message: impl Into<String>) -> axum::response::Response {
    api_error(StatusCode::BAD_REQUEST, "validation_failed", message)
}

/// `GET /api/settings` reads one settings layer. Without a query it reads the
/// served profile resolved over the machine-wide values, so a plain read and
/// a plain save always describe the same state. See #4144.
pub async fn get_settings(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(query): axum::extract::Query<SettingsQuery>,
) -> axum::response::Response {
    let _namespace = state.profile_namespace.read().await;
    let served = state.served_profile();
    let profile = match SettingsLayer::resolve(&query, &served) {
        Ok(SettingsLayer::Profile(name)) => Some(name),
        Ok(SettingsLayer::Machine) => None,
        Err(message) => return bad_request(message),
    };
    // The profile layer reads as what a session actually runs with: the
    // profile's overrides resolved over the machine-wide values.
    let config_result = match profile {
        Some(name) => crate::session::resolve_config(&name),
        None => crate::session::Config::load(),
    };

    match config_result {
        Ok(config) => match serde_json::to_value(&config) {
            Ok(val) => (StatusCode::OK, Json(val)).into_response(),
            Err(e) => {
                tracing::error!(target: "http.api.system", "Settings serialization failed: {}", e);
                api_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "serialize_failed",
                    "Failed to serialize settings",
                )
            }
        },
        Err(e) => {
            tracing::error!(target: "http.api.system", "Settings load failed: {}", e);
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "load_failed",
                "Failed to load settings",
            )
        }
    }
}

/// Route every leaf of a settings patch to the layer `GET /api/settings` reads
/// it from: the profile override where the field may be overridden, the
/// machine-wide store otherwise. Unknown fields and `plugin:<id>` leaves are
/// machine-wide, so the global-scope validation is the one that rejects them.
fn split_patch_by_layer(
    descriptors: &[crate::session::config::settings_schema::FieldDescriptor],
    body: &serde_json::Value,
) -> (serde_json::Value, serde_json::Value) {
    let mut machine = serde_json::Map::new();
    let mut profile = serde_json::Map::new();
    let Some(root) = body.as_object() else {
        return (body.clone(), serde_json::Value::Object(profile));
    };
    for (section, value) in root {
        // A non-object section (the top-level `description` string) has no
        // per-field policy, so it belongs to the machine-wide store.
        let Some(fields) = value.as_object().filter(|f| !f.is_empty()) else {
            machine.insert(section.clone(), value.clone());
            continue;
        };
        for (field, leaf) in fields {
            let overridable = descriptors
                .iter()
                .any(|d| d.section == *section && d.field == *field && d.profile_overridable);
            let target = if overridable {
                &mut profile
            } else {
                &mut machine
            };
            let section_entry = target
                .entry(section.clone())
                .or_insert_with(|| serde_json::json!({}));
            if let Some(obj) = section_entry.as_object_mut() {
                obj.insert(field.clone(), leaf.clone());
            }
        }
    }
    (
        serde_json::Value::Object(machine),
        serde_json::Value::Object(profile),
    )
}

/// A patch with no leaf left to write, whether it arrived empty or had every
/// field stripped as host-only.
fn is_empty_patch(patch: &serde_json::Value) -> bool {
    patch.as_object().is_none_or(|obj| {
        obj.values()
            .all(|v| v.as_object().is_none_or(|f| f.is_empty()))
    })
}

/// Dotted path of the first leaf in a patch, for the message that names the
/// write an elevated session is missing.
fn first_leaf(patch: &serde_json::Value) -> Option<String> {
    let (section, value) = patch.as_object()?.iter().next()?;
    match value.as_object().filter(|f| !f.is_empty()) {
        Some(fields) => fields
            .keys()
            .next()
            .map(|field| format!("{section}.{field}")),
        None => Some(section.clone()),
    }
}

/// Map a schema [`PatchRejection`] to the HTTP response shape the dashboard
/// expects. `elevation_required` mirrors the path-shape gate's 403 so the web
/// client's interceptor fires the passphrase prompt unchanged.
fn reject_response(rej: PatchRejection) -> axum::response::Response {
    let status = StatusCode::from_u16(rej.status_code()).unwrap_or(StatusCode::BAD_REQUEST);
    (
        status,
        Json(serde_json::json!({"error": rej.error_code(), "message": rej.message()})),
    )
        .into_response()
}

/// Apply a validated patch to the machine-wide store. Returns the new config
/// and whether the persisted logging config changed, so the caller can decide
/// whether to reapply the temporary runtime filters.
fn apply_machine_patch(
    patch: &serde_json::Value,
) -> anyhow::Result<(crate::session::Config, bool)> {
    crate::session::update_config(|config| -> anyhow::Result<_> {
        let mut current = serde_json::to_value(&*config)?;
        crate::session::config::settings_schema::merge_json(&mut current, patch);
        // The target editor sends the complete map, including removals.
        if let Some(targets) = patch.pointer("/logging/targets") {
            current["logging"]["targets"] = if targets.is_null() {
                serde_json::json!({})
            } else {
                targets.clone()
            };
        }
        let updated: crate::session::Config = serde_json::from_value(current)?;
        let logging_changed = config.logging.default_level != updated.logging.default_level
            || config.logging.targets != updated.logging.targets;
        *config = updated;
        Ok((config.clone(), logging_changed))
    })
    .and_then(|inner| inner)
}

/// Apply a validated patch onto a profile's sparse override object and persist
/// it. A `null` leaf clears the override (reverting to the inherited value);
/// sections are created lazily so a single-field patch never wipes its
/// siblings. `description` is a top-level string, handled the same way.
fn apply_profile_patch(
    name: &str,
    patch: &serde_json::Value,
) -> anyhow::Result<crate::session::ProfileConfig> {
    // A settings save cannot create a profile.
    crate::session::require_known_profile(name)?;
    crate::session::update_profile_config(name, |config| {
        let mut current = serde_json::to_value(&*config)?;
        if let Some(update_obj) = patch.as_object() {
            for (key, value) in update_obj {
                match value {
                    serde_json::Value::Object(fields) => {
                        for (field, fval) in fields {
                            if fval.is_null() {
                                clear_path(&mut current, key, field);
                            } else if let Some(root) = current.as_object_mut() {
                                let section = root
                                    .entry(key.clone())
                                    .or_insert_with(|| serde_json::json!({}));
                                if let Some(sec) = section.as_object_mut() {
                                    sec.insert(field.clone(), fval.clone());
                                }
                            }
                        }
                    }
                    serde_json::Value::Null => {
                        if let Some(root) = current.as_object_mut() {
                            root.remove(key);
                        }
                    }
                    other => {
                        if let Some(root) = current.as_object_mut() {
                            root.insert(key.clone(), other.clone());
                        }
                    }
                }
            }
        }
        *config = serde_json::from_value(current)?;
        Ok(config.clone())
    })
}

/// Persist a settings patch, routing each leaf to the layer `GET /api/settings`
/// reads it from, and answer with the effective config of the layer written.
pub async fn update_settings(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(query): axum::extract::Query<SettingsQuery>,
    headers: HeaderMap,
    session: Option<Extension<AuthenticatedSession>>,
    loopback: Option<Extension<Principal>>,
    body: Result<Json<serde_json::Value>, axum::extract::rejection::JsonRejection>,
) -> axum::response::Response {
    // CityHall mode exposes only the theme control, which writes through the
    // dedicated `PATCH /api/theme` endpoint; the general settings PATCH stays
    // fully closed so advanced settings cannot be reached. See #7.
    if let Some(resp) = super::cityhall_block(&state) {
        return resp;
    }
    if state.read_only {
        return super::read_only_response();
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };
    let _namespace = state.profile_namespace.read().await;
    let target = match SettingsLayer::resolve(&query, &state.served_profile()) {
        Ok(layer) => layer,
        Err(message) => return bad_request(message),
    };
    if !body.is_object() {
        return bad_request("A settings patch must be a JSON object");
    }
    // An explicit machine-wide save writes the whole body machine-wide, so a
    // profile-overridable field sets the inherited value on purpose. A save
    // addressed at a profile splits, and each leaf lands where a plain read of
    // the same layer would find it.
    let (machine, profile_patch) = match target {
        SettingsLayer::Profile(_) => split_patch_by_layer(&runtime_schema(), &body),
        SettingsLayer::Machine => (body, serde_json::json!({})),
    };
    let profile = match target {
        SettingsLayer::Profile(name) => Some(name),
        SettingsLayer::Machine => None,
    };
    save_settings_patch(
        state.clone(),
        profile,
        machine,
        profile_patch,
        session,
        loopback,
        WriteStamp::from_headers(&headers),
    )
    .await
}

/// Write the two halves of a split settings patch and answer with the
/// effective config of the layer written.
async fn save_settings_patch(
    state: Arc<AppState>,
    profile: Option<String>,
    mut machine: serde_json::Value,
    mut profile_patch: serde_json::Value,
    session: Option<Extension<AuthenticatedSession>>,
    loopback: Option<Extension<Principal>>,
    stamp: Option<WriteStamp>,
) -> axum::response::Response {
    // Machine-wide leaves set what every profile inherits, so they are the
    // privileged half of this surface. The route-level gate cannot see which
    // layer a request writes, so the check belongs here.
    let elevated = handler_elevated(
        &state,
        session.as_deref(),
        authorize(loopback.as_deref(), Capability::SkipElevation),
    )
    .await;
    if !is_empty_patch(&machine) && !elevated {
        return reject_response(PatchRejection::NeedsElevation {
            path: first_leaf(&machine).unwrap_or_else(|| "<root>".to_string()),
            reason: "machine-wide settings change every profile".to_string(),
        });
    }
    // Strip host-execution surfaces (`local_only`: node_path, agent
    // argv/command, status-hook commands) in both halves, so a bundled or
    // echoed-back patch keeps its safe leaves and silently drops the
    // local-only ones (#1692). They can never reach disk from the web.
    strip_local_only(&mut machine);
    strip_local_only(&mut profile_patch);
    let write_profile = profile.is_some() && !is_empty_patch(&profile_patch);
    // Every remaining leaf is checked against the runtime schema (core plus
    // active-plugin `plugin:<id>` sections) in the scope of the layer it lands
    // in: unknown section/field -> 400, bad value -> 400.
    if let Err(rej) = validate_patch_with(&runtime_schema(), &machine, Scope::Global, elevated) {
        return reject_response(rej);
    }
    if write_profile {
        if let Err(rej) = validate_patch(&profile_patch, Scope::Profile, elevated) {
            return reject_response(rej);
        }
    }

    let mut applied = write_gate().await;
    let stale = drop_stale_leaves(&applied, stamp.as_ref(), "", &mut machine)
        | drop_stale_leaves(
            &applied,
            stamp.as_ref(),
            profile.as_deref().unwrap_or(""),
            &mut profile_patch,
        );
    if stale && is_empty_patch(&machine) && is_empty_patch(&profile_patch) {
        return stale_write_response();
    }
    let write_profile = profile.is_some() && !is_empty_patch(&profile_patch);
    // The profile override is written first: a machine-wide failure then leaves
    // the narrower half in place, which is what the partial-save message says.
    if let Some(name) = profile.clone().filter(|_| write_profile) {
        let patch = profile_patch.clone();
        let label = name.clone();
        match tokio::task::spawn_blocking(move || apply_profile_patch(&name, &patch)).await {
            Ok(Ok(_)) => {
                record_applied(&mut applied, stamp.as_ref(), &label, &profile_patch);
            }
            Ok(Err(e)) => {
                tracing::warn!(target: "http.api.system", "Profile settings update failed: {e}");
                return bad_request(format!(
                    "Failed to save profile settings for '{label}': {e}"
                ));
            }
            Err(e) => {
                tracing::error!(target: "http.api.system", "Profile settings update panicked: {e}");
                return internal_error();
            }
        }
    }

    let mut machine_config = None;
    if !is_empty_patch(&machine) {
        // Capture which plugins this patch touches (top-level `plugin:<id>`
        // sections and their changed field keys) BEFORE the rewrite folds them
        // into `plugins.<id>.settings.*`, so we can emit
        // `plugin.settings.changed` after a successful write (#2897).
        let plugin_changes: Vec<(String, Vec<String>)> = machine
            .as_object()
            .map(|obj| {
                obj.iter()
                    .filter_map(|(section, value)| {
                        let id =
                            crate::session::config::settings_schema::section_plugin_id(section)?;
                        let keys: Vec<String> = value
                            .as_object()
                            .map(|m| m.keys().cloned().collect())
                            .unwrap_or_default();
                        Some((id.to_string(), keys))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mut written = machine.clone();
        rewrite_plugin_sections(&mut written);
        match tokio::task::spawn_blocking(move || apply_machine_patch(&written)).await {
            Ok(Ok((config, logging_changed))) => {
                // No-op and restart-only edits preserve temporary runtime filters.
                if logging_changed {
                    if let Ok(app_dir) = crate::session::get_app_dir() {
                        crate::logging::apply_persisted_config(
                            &config.logging.default_level,
                            &config.logging.targets,
                            &app_dir,
                        );
                    }
                }
                // Tell each touched plugin's worker its settings changed (#2897),
                // after the durable write. Best-effort; config.get is the fallback.
                if !plugin_changes.is_empty() {
                    if let Some(host) = &state.plugin_host {
                        host.emit_settings_changed(&plugin_changes, None).await;
                    }
                }
                machine_config = Some(config);
                record_applied(&mut applied, stamp.as_ref(), "", &machine);
            }
            Ok(Err(e)) => {
                tracing::warn!(target: "http.api.system", "Settings update failed: {e}");
                let target = profile.as_deref().unwrap_or("the machine-wide layer");
                let message = if write_profile {
                    format!(
                        "Failed to update machine-wide settings: {e}. The profile layer of \
                         '{target}' was saved."
                    )
                } else {
                    format!("Failed to update machine-wide settings: {e}")
                };
                return api_error(StatusCode::BAD_REQUEST, "update_failed", message);
            }
            Err(e) => {
                tracing::error!(target: "http.api.system", "Settings update panicked: {e}");
                return internal_error();
            }
        }
    }
    drop(applied);

    // Answer with the same layer the caller addressed, so a save and the read
    // that follows it never disagree.
    let effective = match (&profile, machine_config) {
        (Some(name), _) => {
            let name = name.clone();
            tokio::task::spawn_blocking(move || crate::session::resolve_config(&name)).await
        }
        (None, Some(config)) => Ok(Ok(config)),
        (None, None) => tokio::task::spawn_blocking(crate::session::Config::load).await,
    };
    let config = match effective {
        Ok(Ok(config)) => config,
        Ok(Err(e)) => {
            tracing::error!(target: "http.api.system", "Settings read-back failed: {e}");
            return api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "load_failed",
                "Failed to read back settings",
            );
        }
        Err(e) => {
            tracing::error!(target: "http.api.system", "Settings read-back panicked: {e}");
            return internal_error();
        }
    };
    match serde_json::to_value(&config) {
        Ok(val) => (StatusCode::OK, Json(val)).into_response(),
        Err(e) => {
            tracing::error!(target: "http.api.system", "Settings serialization failed: {e}");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "serialize_failed",
                "Failed to serialize settings",
            )
        }
    }
}

/// Header a client stamps on each settings PATCH: `<tab id>.<sequence>`, increasing per tab.
const WRITE_STAMP_HEADER: &str = "x-aoe-write-stamp";

/// Write-gate scope of the global theme; the `@` keeps it apart from profile names.
const THEME_SCOPE: &str = "@theme";

/// Past this many tracked leaves the table is reset rather than grown without bound.
const MAX_TRACKED_WRITES: usize = 4096;

struct WriteStamp {
    tab: String,
    seq: u64,
}

impl WriteStamp {
    fn from_headers(headers: &HeaderMap) -> Option<Self> {
        let (tab, seq) = headers
            .get(WRITE_STAMP_HEADER)?
            .to_str()
            .ok()?
            .rsplit_once('.')?;
        Some(Self {
            tab: tab.to_string(),
            seq: seq.parse().ok()?,
        })
    }

    fn key(&self, scope: &str, section: &str, field: &str) -> String {
        format!("{}|{scope}|{section}.{field}", self.tab)
    }
}

/// Latest stamped write applied per tab and leaf, behind the lock that also serialises the
/// writes themselves. A PATCH whose connection dropped can still commit late; dropping it
/// when a newer write of the same leaf already landed keeps the client's latest edit.
async fn write_gate() -> tokio::sync::MutexGuard<'static, HashMap<String, u64>> {
    static GATE: tokio::sync::OnceCell<tokio::sync::Mutex<HashMap<String, u64>>> =
        tokio::sync::OnceCell::const_new();
    GATE.get_or_init(|| async { tokio::sync::Mutex::new(HashMap::new()) })
        .await
        .lock()
        .await
}

fn patch_leaves(patch: &serde_json::Value) -> Vec<(String, String)> {
    let Some(sections) = patch.as_object() else {
        return Vec::new();
    };
    sections
        .iter()
        .flat_map(|(section, value)| match value.as_object() {
            Some(fields) => fields
                .keys()
                .map(|field| (section.clone(), field.clone()))
                .collect(),
            None => vec![(section.clone(), String::new())],
        })
        .collect()
}

/// Remove leaves an equal or newer stamped write already applied; true if any were removed.
fn drop_stale_leaves(
    applied: &HashMap<String, u64>,
    stamp: Option<&WriteStamp>,
    scope: &str,
    patch: &mut serde_json::Value,
) -> bool {
    let Some(stamp) = stamp else {
        return false;
    };
    let mut dropped = false;
    for (section, field) in patch_leaves(patch) {
        if applied
            .get(&stamp.key(scope, &section, &field))
            .is_some_and(|&seen| seen >= stamp.seq)
        {
            dropped = true;
            if field.is_empty() {
                patch.as_object_mut().map(|m| m.remove(&section));
            } else if let Some(fields) = patch.get_mut(&section).and_then(|v| v.as_object_mut()) {
                fields.remove(&field);
                if fields.is_empty() {
                    patch.as_object_mut().map(|m| m.remove(&section));
                }
            }
        }
    }
    dropped
}

fn record_applied(
    applied: &mut HashMap<String, u64>,
    stamp: Option<&WriteStamp>,
    scope: &str,
    patch: &serde_json::Value,
) {
    let Some(stamp) = stamp else {
        return;
    };
    if applied.len() > MAX_TRACKED_WRITES {
        applied.clear();
    }
    for (section, field) in patch_leaves(patch) {
        applied.insert(stamp.key(scope, &section, &field), stamp.seq);
    }
}

fn stale_write_response() -> axum::response::Response {
    api_error(
        StatusCode::CONFLICT,
        "stale_write",
        "A newer save of these settings was already applied",
    )
}

fn internal_error() -> axum::response::Response {
    api_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal",
        "Internal server error",
    )
}

/// `GET /api/cityhall/bundle` returns this install's CityHall config bundle as
/// TOML, for an admin to paste into CityHall. See
/// `crate::session::cityhall_bundle`.
///
/// Refused in CityHall client mode: this is the surface an admin uses to
/// configure workspaces, and an end user inside one has no business reading it.
/// `cityhall_gate` only guards mutations, so a read needs its own block.
pub async fn get_cityhall_bundle(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> axum::response::Response {
    if let Some(resp) = super::cityhall_block(&state) {
        return resp;
    }
    let built = tokio::task::spawn_blocking(|| {
        crate::session::cityhall_bundle::export().and_then(|b| b.to_toml())
    })
    .await;
    match built {
        Ok(Ok(toml)) => (
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "application/toml")],
            toml,
        )
            .into_response(),
        Ok(Err(e)) => {
            tracing::error!(target: "http.api.system", "CityHall bundle export failed: {e}");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "export_failed",
                e.to_string(),
            )
        }
        Err(e) => {
            tracing::error!(target: "http.api.system", "CityHall bundle export panicked: {e}");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "Internal server error",
            )
        }
    }
}

/// `GET /api/settings/schema` returns the flat list of settings field
/// descriptors (the single source of truth, see #1692). The web dashboard
/// renders a generic field component from this list instead of hand-written
/// per-field JSX, so a new config field appears on the web automatically. No
/// secrets: descriptors are pure metadata (labels, widgets, validation, write
/// policy), so this needs no elevation, only normal authentication.
pub async fn get_settings_schema(
) -> Json<Vec<crate::session::config::settings_schema::FieldDescriptor>> {
    Json(runtime_schema())
}

/// `GET /api/settings/resolved` returns every setting's effective value plus
/// its provenance chain (user value > highest-priority plugin default > schema
/// default for core; stored value > manifest default for plugin settings). The
/// dashboard uses it to show where a value comes from. Pure metadata derived
/// from the same schema the surfaces render, so only normal authentication.
pub async fn get_settings_resolved(
) -> Json<Vec<crate::session::config::settings_schema::ResolvedSetting>> {
    Json(
        tokio::task::spawn_blocking(crate::session::config::settings_schema::resolve_all)
            .await
            .unwrap_or_default(),
    )
}

/// Body of `PATCH /api/theme`. Either field may be omitted to leave it
/// unchanged.
#[derive(serde::Deserialize)]
pub struct ThemePatch {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub color_mode: Option<crate::session::config::ColorMode>,
}

/// `PATCH /api/theme` sets the global theme name and/or color mode and returns
/// the freshly resolved theme so the caller can repaint.
///
/// The theme is a global preference (see `config::resolve_theme_name`): it
/// lives in the global config, never a profile, so one theme paints every
/// surface. This is a dedicated, non-elevated write (like the web-tour flag)
/// rather than routing through `PATCH /api/settings`: a cosmetic theme change
/// must not trip the passphrase wall that guards the general settings surface
/// (`requires_elevation` keeps profile-settings preference writes off that wall
/// for the same reason). `read_only` still blocks it.
pub async fn update_theme(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Result<Json<ThemePatch>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return super::read_only_response();
    }
    let Json(mut patch) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };
    // CityHall hides the color-mode control, so only the name is writable (#7).
    if state.cityhall_mode {
        patch.color_mode = None;
    }
    // Reject an unknown name so a typo cannot silently repaint to `default`.
    // Empty is allowed and clears back to the default builtin.
    if let Some(name) = &patch.name {
        if !name.is_empty()
            && !crate::tui::styles::available_themes()
                .iter()
                .any(|t| t == name)
        {
            return api_error(
                StatusCode::BAD_REQUEST,
                "unknown_theme",
                format!("Unknown theme '{name}'"),
            );
        }
    }
    // Same late-write guard as settings saves: drop a stamped leaf a newer write already applied.
    let stamp = WriteStamp::from_headers(&headers);
    let mut applied = write_gate().await;
    let mut leaves = serde_json::Map::new();
    if let Some(name) = &patch.name {
        leaves.insert("name".into(), name.clone().into());
    }
    if patch.color_mode.is_some() {
        leaves.insert("color_mode".into(), true.into());
    }
    let mut written = serde_json::json!({ "theme": leaves });
    if drop_stale_leaves(&applied, stamp.as_ref(), THEME_SCOPE, &mut written) {
        if is_empty_patch(&written) {
            return stale_write_response();
        }
        let kept = &written["theme"];
        if kept.get("name").is_none() {
            patch.name = None;
        }
        if kept.get("color_mode").is_none() {
            patch.color_mode = None;
        }
    }
    let result = tokio::task::spawn_blocking(move || {
        // `update_config` re-loads via `Config::load()`, so a corrupt
        // config.toml errors out instead of being replaced with defaults.
        let theme_name = crate::session::update_config(|config| {
            if let Some(name) = patch.name {
                config.theme.name = name;
            }
            if let Some(mode) = patch.color_mode {
                config.theme.color_mode = mode;
            }
            config.effective_theme_name()
        })?;
        Ok::<_, anyhow::Error>(crate::tui::styles::resolve_theme(&theme_name))
    })
    .await;
    if matches!(result, Ok(Ok(_))) {
        record_applied(&mut applied, stamp.as_ref(), THEME_SCOPE, &written);
    }
    drop(applied);

    match result {
        Ok(Ok(resolved)) => match serde_json::to_value(&resolved) {
            Ok(val) => (StatusCode::OK, Json(val)).into_response(),
            Err(e) => {
                tracing::error!(target: "http.api.system", "theme serialization failed: {}", e);
                api_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "serialize_failed",
                    "Failed to serialize theme",
                )
            }
        },
        Ok(Err(e)) => {
            tracing::warn!(target: "http.api.system", "theme update failed: {}", e);
            api_error(
                StatusCode::BAD_REQUEST,
                "update_failed",
                "Failed to update theme",
            )
        }
        Err(e) => {
            tracing::error!(target: "http.api.system", "theme update panicked: {}", e);
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "Internal server error",
            )
        }
    }
}

/// Marks the web dashboard's first-run tour as seen for this server.
///
/// Single-purpose write so the cosmetic flag never widens the
/// `PATCH /api/settings` surface (which carries security-sensitive
/// sections like `sandbox`/`worktree`). Deliberately exempt from the
/// elevation/passphrase wall: it flips one cosmetic bool, grants no
/// capability, and `read_only` still blocks it. Persisted via
/// `update_app_state` into `state.toml`, entirely separate from
/// `config.toml`, so a corrupt global config can never block this flag.
pub async fn mark_web_tour_seen(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    if state.read_only {
        return super::read_only_response();
    }

    let result = tokio::task::spawn_blocking(|| {
        crate::session::update_app_state(|state| {
            state.has_seen_web_tour = true;
        })
    })
    .await;

    match result {
        Ok(Ok(())) => (
            StatusCode::OK,
            Json(serde_json::json!({"has_seen_web_tour": true})),
        )
            .into_response(),
        Ok(Err(e)) => {
            tracing::warn!(target: "http.api.system", "Marking web tour seen failed: {}", e);
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "save_failed",
                "Failed to persist tour state",
            )
        }
        Err(e) => {
            tracing::error!(target: "http.api.system", "Marking web tour seen panicked: {}", e);
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "Internal server error",
            )
        }
    }
}

#[derive(Serialize)]
pub struct TipDto {
    pub id: String,
    pub title: String,
    pub body: String,
    pub seen: bool,
}

#[derive(Serialize)]
pub struct TipsResponse {
    /// Mirror of `session.show_tips`. The dashboard hides the badge and panel
    /// when this is false. This payload is a read projection; the toggle is
    /// written through the dedicated `POST /api/tips/show` ([`set_show_tips`]),
    /// and the same preference is also editable from the settings schema.
    pub enabled: bool,
    /// Web-eligible tips in catalog order, each flagged with whether it has been
    /// seen. The frontend derives the badge count from the unseen ones and can
    /// still show seen tips in a collapsed section.
    pub tips: Vec<TipDto>,
}

/// Returns the web-surface tips and whether tips are enabled, composed from the
/// shared `crate::tips` catalog plus `app_state.tips_seen` and
/// `session.show_tips`. A read projection, so it stays a plain GET behind the
/// token wall like [`get_web_ui_state`]; the TUI-only tips never appear here.
pub async fn get_tips(State(_state): State<Arc<AppState>>) -> impl IntoResponse {
    let result = tokio::task::spawn_blocking(|| {
        let config = crate::session::Config::load()?;
        let signals = crate::tips::TipSignals::from(&config);
        let seen = &config.app_state.tips_seen;
        let tips = crate::tips::eligible(crate::tips::TipSurface::Web, &signals)
            .into_iter()
            .map(|tip| TipDto {
                id: tip.id.to_string(),
                title: tip.title.to_string(),
                body: tip.body.to_string(),
                seen: seen.iter().any(|s| s == tip.id),
            })
            .collect();
        Ok::<_, anyhow::Error>(TipsResponse {
            enabled: config.session.show_tips,
            tips,
        })
    })
    .await;

    match result {
        Ok(Ok(resp)) => (StatusCode::OK, Json(resp)).into_response(),
        // Best-effort: an unreadable config yields an empty, disabled payload
        // so the dashboard shows no badge rather than erroring.
        _ => (
            StatusCode::OK,
            Json(TipsResponse {
                enabled: false,
                tips: Vec::new(),
            }),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
pub struct MarkTipSeenBody {
    pub id: String,
}

/// Marks one tip seen by appending its id to the shared `app_state.tips_seen`
/// in `state.toml`, so the dashboard's mark-seen-on-view sticks across devices
/// and matches the TUI. Single-purpose write mirroring [`mark_web_tour_seen`]:
/// exempt from the elevation wall, still blocked by `read_only`. Rejects an id
/// that is not in the catalog so junk can't accumulate in the persisted seen
/// list.
pub async fn mark_tip_seen(
    State(state): State<Arc<AppState>>,
    body: Result<Json<MarkTipSeenBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return super::read_only_response();
    }
    let Json(MarkTipSeenBody { id }) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };
    if !crate::tips::id_in_catalog(&id) {
        return api_error(
            StatusCode::BAD_REQUEST,
            "unknown_tip",
            format!("Unknown tip id '{id}'"),
        );
    }

    let result = tokio::task::spawn_blocking(move || {
        crate::session::update_app_state(|state| {
            if !state.tips_seen.iter().any(|s| s == &id) {
                state.tips_seen.push(id);
            }
        })
    })
    .await;

    match result {
        Ok(Ok(())) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response(),
        Ok(Err(e)) => {
            tracing::warn!(target: "http.api.system", "Marking tip seen failed: {}", e);
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "save_failed",
                "Failed to persist tip state",
            )
        }
        Err(e) => {
            tracing::error!(target: "http.api.system", "Marking tip seen panicked: {}", e);
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "Internal server error",
            )
        }
    }
}

#[derive(Deserialize)]
pub struct SetShowTipsBody {
    pub enabled: bool,
}

/// Sets `session.show_tips`, the "Show tips on startup" checkbox in the tip-of-
/// the-day modal. A dedicated single-purpose write rather than `PATCH
/// /api/settings`, which the auth middleware elevation-gates: this is a cosmetic
/// preference and must not trip the passphrase wall on a remote server. Mirrors
/// [`mark_web_tour_seen`]: exempt from elevation, still blocked by `read_only`,
/// and uses `Config::load()` so a corrupt config is not silently replaced. The
/// same preference is also editable from the settings Interaction tab.
pub async fn set_show_tips(
    State(state): State<Arc<AppState>>,
    body: Result<Json<SetShowTipsBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return super::read_only_response();
    }
    let Json(SetShowTipsBody { enabled }) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };

    let result = tokio::task::spawn_blocking(move || {
        crate::session::update_config(|config| {
            config.session.show_tips = enabled;
        })
    })
    .await;

    match result {
        Ok(Ok(())) => (
            StatusCode::OK,
            Json(serde_json::json!({"show_tips": enabled})),
        )
            .into_response(),
        Ok(Err(e)) => {
            tracing::warn!(target: "http.api.system", "Setting show_tips failed: {}", e);
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "save_failed",
                "Failed to persist tips state",
            )
        }
        Err(e) => {
            tracing::error!(target: "http.api.system", "Setting show_tips panicked: {}", e);
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "Internal server error",
            )
        }
    }
}

#[derive(serde::Deserialize)]
pub struct DismissUpdateBody {
    pub version: String,
}

/// Records that the user dismissed the update banner for a specific version,
/// persisting to the shared `app_state.dismissed_update_version` in
/// `state.toml` so the dismissal sticks across devices (and matches the
/// TUI's snooze) rather than living in per-browser localStorage.
/// Single-purpose write mirroring [`mark_web_tour_seen`]: exempt from the
/// elevation wall, still blocked by `read_only`.
pub async fn dismiss_update(
    State(state): State<Arc<AppState>>,
    body: Result<Json<DismissUpdateBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return super::read_only_response();
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };
    let version = body.version;
    let persisted = version.clone();

    let result = tokio::task::spawn_blocking(move || {
        crate::session::update_app_state(|state| {
            state.dismissed_update_version = Some(persisted);
        })
    })
    .await;

    match result {
        Ok(Ok(())) => (
            StatusCode::OK,
            Json(serde_json::json!({"dismissed_version": version})),
        )
            .into_response(),
        Ok(Err(e)) => {
            tracing::warn!(target: "http.api.system", "Dismissing update failed: {}", e);
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "save_failed",
                "Failed to persist dismissal",
            )
        }
        Err(e) => {
            tracing::error!(target: "http.api.system", "Dismissing update panicked: {}", e);
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "Internal server error",
            )
        }
    }
}

/// Return the web dashboard's server-side UI-state blob (`app_state.web_ui_state`):
/// a flat map of the frontend's localStorage keys to their opaque string values.
/// Single-tenant, so this is the one user's synced prefs. GET is unauthenticated
/// beyond the normal token wall (it grants no capability and exposes only UI
/// preferences).
pub async fn get_web_ui_state(State(_state): State<Arc<AppState>>) -> impl IntoResponse {
    let result = tokio::task::spawn_blocking(|| {
        let config = crate::session::Config::load()?;
        Ok::<_, anyhow::Error>(config.app_state.web_ui_state)
    })
    .await;
    match result {
        Ok(Ok(map)) => (StatusCode::OK, Json(serde_json::json!(map))).into_response(),
        // Best-effort: an unreadable config yields an empty blob, so the
        // dashboard falls back to its localStorage cache.
        _ => (StatusCode::OK, Json(serde_json::json!({}))).into_response(),
    }
}

/// Merge a partial update into `app_state.web_ui_state`. The body is a flat JSON
/// object keyed by the frontend's localStorage keys: a string value sets the
/// key, `null` deletes it. Mirrors [`mark_web_tour_seen`]'s exemptions (off the
/// elevation wall, still blocked by `read_only`, `Config::load()` to avoid
/// clobbering a corrupt config). Non-string, non-null values are ignored since
/// localStorage values are always strings.
pub async fn patch_web_ui_state(
    State(state): State<Arc<AppState>>,
    body: Result<
        Json<serde_json::Map<String, serde_json::Value>>,
        axum::extract::rejection::JsonRejection,
    >,
) -> impl IntoResponse {
    if state.read_only {
        return super::read_only_response();
    }
    let Json(patch) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };

    // Reject anything that is not a string (set) or null (delete), so a client
    // regression surfaces instead of silently dropping part of the sync.
    let invalid: Vec<&String> = patch
        .iter()
        .filter(|(_, v)| !v.is_string() && !v.is_null())
        .map(|(k, _)| k)
        .collect();
    if !invalid.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "invalid_value_type",
                "message": "web UI state values must be string or null",
                "keys": invalid,
            })),
        )
            .into_response();
    }

    let result = tokio::task::spawn_blocking(move || {
        crate::session::update_app_state(|state| {
            for (key, value) in patch {
                match value {
                    serde_json::Value::Null => {
                        state.web_ui_state.remove(&key);
                    }
                    serde_json::Value::String(s) => {
                        state.web_ui_state.insert(key, s);
                    }
                    // Already rejected above; kept exhaustive.
                    _ => {}
                }
            }
        })
    })
    .await;

    match result {
        Ok(Ok(())) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response(),
        Ok(Err(e)) => {
            tracing::warn!(target: "http.api.system", "Persisting web UI state failed: {}", e);
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "save_failed",
                "Failed to persist UI state",
            )
        }
        Err(e) => {
            tracing::error!(target: "http.api.system", "Persisting web UI state panicked: {}", e);
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "Internal server error",
            )
        }
    }
}

/// Records that the user has acknowledged glob `volume_ignores` snapshot
/// expansion (#2045), so the new-session wizard's confirm modal is shown once
/// and never again. Single-purpose write mirroring [`mark_web_tour_seen`]: it
/// flips one bool, grants no capability, stays exempt from the elevation wall,
/// and `read_only` still blocks it.
pub async fn mark_volume_ignores_globs_acknowledged(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    if state.read_only {
        return super::read_only_response();
    }

    let result = tokio::task::spawn_blocking(|| {
        crate::session::update_app_state(|state| {
            state.has_acknowledged_volume_ignores_globs = true;
        })
    })
    .await;

    match result {
        Ok(Ok(())) => (
            StatusCode::OK,
            Json(serde_json::json!({"has_acknowledged_volume_ignores_globs": true})),
        )
            .into_response(),
        Ok(Err(e)) => {
            tracing::warn!(target: "http.api.system", "Marking volume_ignores globs acknowledged failed: {}", e);
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "save_failed",
                "Failed to persist acknowledgment",
            )
        }
        Err(e) => {
            tracing::error!(target: "http.api.system", "Marking volume_ignores globs acknowledged panicked: {}", e);
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "Internal server error",
            )
        }
    }
}

// --- Themes ---

pub async fn list_themes() -> Json<Vec<String>> {
    Json(
        crate::tui::styles::available_themes()
            .into_iter()
            .map(|s| s.to_string())
            .collect(),
    )
}

/// Upper bound on the `:name` path segment for `/api/themes/:name`.
/// Builtin names are <= 20 chars and custom theme filenames are
/// inherently capped by the host filesystem; 128 is far past any
/// real theme name. Past the cap we resolve Empire without logging
/// the body to keep tracing output sane under fuzzing.
const MAX_THEME_NAME_LEN: usize = 128;

/// `GET /api/themes/:name` returns the resolved theme projection (web
/// CSS vars, terminal CSS vars, syntax highlighter selection,
/// appearance) for the named theme. Unknown names resolve to the
/// `default` builtin with `source: "fallback"`, mirroring
/// `load_theme`'s behaviour.
///
/// Wrapped in `spawn_blocking`: the resolver does sync file I/O
/// (`discover_custom_themes` directory scan + TOML parse) which must
/// not run on a tokio worker thread.
pub async fn get_resolved_theme(
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Json<crate::tui::styles::ResolvedTheme> {
    if name.len() > MAX_THEME_NAME_LEN {
        tracing::warn!(
            len = name.len(),
            "GET /api/themes/{{name}} rejected: name exceeds {} bytes",
            MAX_THEME_NAME_LEN,
        );
        return Json(crate::tui::styles::resolve_theme("zinc"));
    }
    tracing::debug!(theme = %name, "GET /api/themes/{{name}}");
    let resolved = tokio::task::spawn_blocking(move || crate::tui::styles::resolve_theme(&name))
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "theme resolve task panicked, falling back to default");
            crate::tui::styles::resolve_theme("zinc")
        });
    Json(resolved)
}

/// `GET /api/theme/current` returns the resolved theme to paint. Theme is a
/// global preference, not profile-merged, so this reads the global config
/// (see `config::resolve_theme_name`); every surface paints the same theme
/// regardless of the active session profile. Sync work runs in
/// `spawn_blocking`.
pub async fn get_current_theme(
    State(state): State<Arc<AppState>>,
) -> Json<crate::tui::styles::ResolvedTheme> {
    tracing::debug!(target: "http.api.system", profile = %*state.served_profile(), "GET /api/theme/current");
    let resolved = tokio::task::spawn_blocking(move || {
        let name = crate::session::config::resolve_theme_name();
        crate::tui::styles::resolve_theme(&name)
    })
    .await
    .unwrap_or_else(|e| {
        tracing::warn!(error = %e, "current theme resolve task panicked, falling back to default");
        crate::tui::styles::resolve_theme("zinc")
    });
    Json(resolved)
}

// --- Wizard support ---

#[derive(Serialize)]
pub struct ProfileInfo<'a> {
    pub name: &'a str,
    pub is_default: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<&'a str>,
}

impl AsRef<str> for ProfileInfo<'_> {
    fn as_ref(&self) -> &str {
        self.name
    }
}

pub async fn list_profiles(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let snapshot = match state.runtime.snapshot(&state).await {
        Ok(snapshot) => snapshot,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let mut profiles: Vec<_> = snapshot
        .value
        .contents
        .profiles
        .iter()
        .map(|profile| ProfileInfo {
            name: &profile.name,
            is_default: profile.name == snapshot.value.contents.default_profile,
            description: profile.description.as_deref(),
        })
        .collect();
    crate::session::sort_profiles_for_display(&mut profiles);
    Json(profiles).into_response()
}

#[derive(Deserialize)]
pub struct BrowseQuery {
    pub path: String,
    pub limit: Option<usize>,
    pub filter: Option<String>,
    /// Include dotfile-prefixed directories in the listing. Mirrors the TUI
    /// picker's Ctrl+H toggle (`src/tui/components/dir_picker.rs`). Omitted
    /// means false, so existing callers keep the old behavior.
    #[serde(default)]
    pub show_hidden: bool,
}

#[derive(Serialize)]
pub struct DirEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub is_git_repo: bool,
}

#[derive(Serialize)]
struct BrowseResponse {
    entries: Vec<DirEntry>,
    has_more: bool,
}

pub async fn filesystem_home(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    if let Some(resp) = super::cityhall_block(&state) {
        return resp;
    }
    match dirs::home_dir() {
        Some(home) => (
            StatusCode::OK,
            Json(serde_json::json!({"path": home.to_string_lossy()})),
        )
            .into_response(),
        None => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "Could not determine home directory"})),
        )
            .into_response(),
    }
}

/// Standard system folders that live directly under $HOME. On macOS several
/// of these (Downloads, Desktop, Pictures/Photos, Music) are TCC-protected:
/// opening them triggers a permission prompt. None is ever a git repo, so the
/// directory browser skips its `.git` probe for them and avoids the prompt.
const HOME_SYSTEM_DIRS: &[&str] = &[
    "Desktop",
    "Documents",
    "Downloads",
    "Movies",
    "Music",
    "Pictures",
    "Public",
    "Library",
];

/// The TCC prompt only exists on macOS, so gate the skip there. On other
/// platforms these folders are probed normally and keep their git badge.
fn skip_git_probe(parent: &std::path::Path, name: &str, home: Option<&std::path::Path>) -> bool {
    if !cfg!(target_os = "macos") {
        return false;
    }
    match home {
        Some(home) => parent == home && HOME_SYSTEM_DIRS.contains(&name),
        None => false,
    }
}

pub async fn browse_filesystem(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(query): axum::extract::Query<BrowseQuery>,
) -> impl IntoResponse {
    if let Some(resp) = super::cityhall_block(&state) {
        return resp;
    }
    let result = tokio::task::spawn_blocking(move || {
        let limit = query.limit.unwrap_or(100);
        let filter = query.filter.map(|f| f.trim().to_lowercase());
        let path = std::path::Path::new(&query.path);
        let canonical = path.canonicalize().map_err(|_| "Path does not exist")?;

        if !canonical.is_dir() {
            return Err("Path is not a directory");
        }

        let home = dirs::home_dir();
        // Security: restrict browsing to the user's home directory
        if let Some(home) = &home {
            if !canonical.starts_with(home) {
                return Err("Path is outside the home directory");
            }
        }

        let mut entries: Vec<DirEntry> = Vec::new();
        let read_dir = std::fs::read_dir(&canonical).map_err(|_| "Cannot read directory")?;

        for entry in read_dir.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !query.show_hidden && name.starts_with('.') {
                continue;
            }
            let entry_path = entry.path();
            let is_dir = entry_path.is_dir();
            if !is_dir {
                continue;
            }
            if let Some(filter) = filter.as_deref() {
                if !filter.is_empty() && !name.to_lowercase().contains(filter) {
                    continue;
                }
            }
            // Probing `.git` opens the directory, which prompts under macOS
            // TCC. None of the standard $HOME folders is ever a repo.
            let is_git_repo = if skip_git_probe(&canonical, &name, home.as_deref()) {
                false
            } else {
                entry_path.join(".git").exists()
            };
            entries.push(DirEntry {
                name,
                path: entry_path.to_string_lossy().to_string(),
                is_dir,
                is_git_repo,
            });
        }
        // Cached: `sort_by_cached_key` calls the keyfn O(n) times rather than
        // O(n log n), so the lowercase String is allocated once per entry.
        entries.sort_by_cached_key(|e| e.name.to_lowercase());
        let has_more = entries.len() > limit;
        entries.truncate(limit);
        Ok(BrowseResponse { entries, has_more })
    })
    .await;

    match result {
        Ok(Ok(resp)) => (StatusCode::OK, Json(serde_json::to_value(resp).unwrap())).into_response(),
        Ok(Err(msg)) => api_error(StatusCode::BAD_REQUEST, "browse_failed", msg),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    }
}

#[derive(Debug, PartialEq, Serialize)]
pub struct GroupInfo {
    pub path: String,
    /// Sessions directly in this group, not in its children.
    pub session_count: usize,
    pub archived: bool,
}

#[derive(Deserialize)]
pub struct GroupsQuery {
    profile: Option<String>,
}

/// `GET /api/groups[?profile=]`: the profile's groups for the wizard's Group field.
pub async fn list_groups(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(query): axum::extract::Query<GroupsQuery>,
) -> impl IntoResponse {
    let _namespace = state.profile_namespace.read().await;
    let (profile, stored) = {
        let metadata = state.canonical_metadata.read().await;
        let served = state.served_profile().to_string();
        let name = query.profile.filter(|p| !p.is_empty()).unwrap_or_else(|| {
            if served.is_empty() {
                metadata.default_profile.clone()
            } else {
                served
            }
        });
        let Some(profile) = metadata.profiles.iter().find(|p| p.name == name) else {
            return Json(Vec::<GroupInfo>::new());
        };
        (profile.name.clone(), profile.groups.clone())
    };
    let session_groups: Vec<String> = state
        .instances
        .read()
        .await
        .iter()
        .filter(|inst| {
            inst.source_profile == profile
                && inst.trashed_at.is_none()
                && inst.archived_at.is_none()
        })
        .map(|inst| inst.group_path.clone())
        .collect();
    Json(group_infos(&session_groups, &stored))
}

/// Every group in the profile's tree (stored groups, the groups sessions are in,
/// and each parent of a nested path), with its direct session count, sorted by
/// path. The synthetic Archived, Trash and Scratch sections are left out.
fn group_infos(session_groups: &[String], stored: &[crate::session::Group]) -> Vec<GroupInfo> {
    let tree = crate::session::GroupTree::new_with_group_paths(
        session_groups.iter().map(String::as_str),
        stored,
    );
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for path in session_groups {
        *counts.entry(path.as_str()).or_default() += 1;
    }
    let mut groups: Vec<GroupInfo> = tree
        .get_all_groups()
        .into_iter()
        .filter(|g| !g.path.is_empty() && !crate::session::is_synthetic_project_header(&g.path))
        .map(|g| GroupInfo {
            session_count: counts.get(g.path.as_str()).copied().unwrap_or_default(),
            archived: g.is_archived(),
            path: g.path,
        })
        .collect();
    groups.sort_by(|a, b| a.path.cmp(&b.path));
    groups
}

/// One agent row on the health readout. Every figure is optional for the same
/// reason as on `AgentMetric`: a sandboxed agent's numbers come from the
/// container runtime, which may have no sample to give.
#[derive(Serialize)]
pub struct SystemHealthAgent {
    pub id: String,
    pub title: String,
    pub cpu_fraction: Option<f64>,
    pub memory_bytes: Option<u64>,
    pub procs: Option<usize>,
    pub sandboxed: bool,
}

/// Host headroom plus per-agent usage, as the system-health strip reads it.
/// `status` is the server's own worst-of classification, so both dashboards
/// band a reading identically rather than each re-deriving the thresholds.
#[derive(Serialize)]
pub struct SystemHealth {
    pub status: &'static str,
    pub cpu_fraction: Option<f64>,
    pub memory_used_bytes: u64,
    pub memory_total_bytes: u64,
    pub load_average: Option<[f64; 3]>,
    pub swap_used_bytes: u64,
    pub swap_total_bytes: u64,
    pub agent_count: usize,
    pub proc_count: usize,
    pub agents: Vec<SystemHealthAgent>,
}

pub async fn system_health(State(state): State<Arc<AppState>>) -> axum::response::Response {
    use crate::process::metrics::pressure_band;

    // The per-agent rows are the population CityHall hides: the sampler's
    // `eligible_instance` selects the non-structured sessions that
    // `list_sessions` filters out and `sessions/search` refuses, so serving
    // this would hand a locked-down client their ids and titles. See #7.
    if let Some(resp) = super::cityhall_block(&state) {
        return resp;
    }

    let instances = state.instances.read().await.clone();
    // Sampling walks the host process table, shells out to tmux, and may read
    // container stats, so it runs off the async workers: a tmux stall would
    // otherwise hold one for as long as its timeout. The sampler's lock is
    // taken inside that work, not around it, so two concurrent polls still
    // cannot interleave their CPU deltas and report nonsense to both.
    let sampler_state = Arc::clone(&state);
    let snapshot = tokio::task::spawn_blocking(move || {
        let mut sampler = sampler_state.metrics_sampler.blocking_lock();
        sampler.sample(&instances)
    })
    .await
    .unwrap_or_default();

    Json(SystemHealth {
        status: pressure_band(&snapshot.memory).as_str(),
        cpu_fraction: snapshot.system.cpu_fraction,
        memory_used_bytes: snapshot.memory.used_bytes(),
        memory_total_bytes: snapshot.memory.total_bytes,
        load_average: snapshot.system.load_average,
        swap_used_bytes: snapshot.system.swap_used_bytes,
        swap_total_bytes: snapshot.system.swap_total_bytes,
        agent_count: snapshot.counts.agents,
        proc_count: snapshot.counts.procs,
        agents: snapshot
            .agents
            .into_iter()
            .map(|a| SystemHealthAgent {
                id: a.id,
                title: a.title,
                cpu_fraction: a.cpu_fraction,
                memory_bytes: a.rss_bytes,
                procs: a.procs,
                sandboxed: a.sandboxed,
            })
            .collect(),
    })
    .into_response()
}

#[derive(Serialize)]
pub struct DockerStatus {
    pub available: bool,
    pub runtime: Option<String>,
}

pub async fn docker_status() -> Json<DockerStatus> {
    let result = tokio::task::spawn_blocking(|| {
        let runtime = crate::containers::get_container_runtime();
        let available = runtime.is_available() && runtime.is_daemon_running();
        let runtime_name = if available {
            let config = crate::session::Config::load_or_warn();
            Some(
                serde_json::to_value(config.sandbox.container_runtime)
                    .ok()
                    .and_then(|v| v.as_str().map(String::from))
                    .unwrap_or_else(|| "docker".to_string()),
            )
        } else {
            None
        };
        DockerStatus {
            available,
            runtime: runtime_name,
        }
    })
    .await
    .unwrap_or(DockerStatus {
        available: false,
        runtime: None,
    });
    Json(result)
}

/// Read-only runtime view of the `aoe serve` daemon's sleep-inhibit reconciler.
/// Derived from a snapshot the poll loop publishes plus the live backend latch;
/// never a control surface.
#[derive(Serialize)]
pub struct SleepInhibitStatus {
    /// The `session.prevent_sleep_when_active` toggle as the reconciler last
    /// read it: the raw config toggle only, not the reconciler's `desired`
    /// (which also folds in recent activity), nor whether an assertion is held.
    pub prevent_sleep_enabled: bool,
    /// Whether the daemon is holding an OS sleep assertion, as of the last
    /// reconcile. Refreshed on the poll loop's interval, so it can trail the
    /// death of the backing child (an external kill, or a backend that spawns
    /// then fails, as on WSL2 with no logind) by up to that interval. Requires
    /// both a retained inhibitor slot and an available backend, so a slot
    /// lingering under the unavailable latch does not report held.
    pub currently_held: bool,
    /// Whether a real OS backend is still believed able to hold the assertion
    /// on this host. Optimistic: `true` means no failure has latched yet, not
    /// that the backend was verified working. It is never actively probed, so
    /// while the toggle is off the backend is never exercised and this stays
    /// `true` even on a host where it would fail. `false` once a failure
    /// latches, and false on unsupported platforms.
    pub backend_available: bool,
}

/// Fold the reconciler snapshot bits and the live backend-availability read into
/// the reported status. `currently_held` gates the retained slot on the backend
/// being available, so the unavailable latch (which keeps a doomed slot around
/// to suppress respawns) and the no-op platform backend never report held.
fn derive_sleep_inhibit_status(
    prevent_sleep_enabled: bool,
    slot_present: bool,
    backend_available: bool,
) -> SleepInhibitStatus {
    SleepInhibitStatus {
        prevent_sleep_enabled,
        currently_held: slot_present && backend_available,
        backend_available,
    }
}

#[derive(Serialize)]
pub struct ServerAbout {
    pub version: String,
    pub auth_required: bool,
    pub passphrase_enabled: bool,
    /// Resolved value of `--auth`: `"token"`, `"passphrase"`, or
    /// `"none"`. The frontend Security panel renders the explicit mode
    /// label off this so `--auth=passphrase` is not mislabeled as
    /// `--no-auth`. Derived from `token_manager.is_no_auth()` plus
    /// `login_manager.is_enabled()` because the CLI mode is not
    /// retained in `AppState` (only its effects are).
    pub auth_mode: &'static str,
    pub read_only: bool,
    pub behind_tunnel: bool,
    /// CityHall client mode (`AOE_CITYHALL_MODE`). Drives the web
    /// dashboard's locked-down end-user client: composer + structured
    /// view only, name-only session creation, theme-only settings, and
    /// no terminal / diff / project-management surfaces. See #7.
    pub cityhall_mode: bool,
    pub profile: String,
    /// Resolved value of `acp.show_tool_durations` from the active
    /// profile's config. Drives the per-tool elapsed-time label in the
    /// web UI; cross-device since it lives in config.toml.
    pub acp_show_tool_durations: bool,
    /// Resolved `acp.wrap_tool_output`: the initial line-wrap state of tool
    /// output blocks in the web UI.
    pub acp_wrap_tool_output: bool,
    /// Resolved value of `acp.replay_events` from the active
    /// profile's config. Per-session retention cap on the acp
    /// event log; 0 means unlimited. The web client mirrors this on
    /// its in-memory activity buffer so the rendered transcript
    /// honours the user's chosen ceiling instead of clipping at a
    /// hard-coded constant. See #1111.
    pub acp_replay_events: u32,
    /// Resolved value of `acp.compaction_reminder` from the active
    /// profile. Gates the structured view's compaction reminder; off by
    /// default. See #3253.
    pub acp_compaction_reminder: bool,
    /// Resolved value of `acp.compaction_reminder_percent` from the
    /// active profile. Context-window percentage at which the reminder
    /// appears.
    pub acp_compaction_reminder_percent: u8,
    /// Resolved `acp.notice_display`: how the structured view shows agent notices.
    pub acp_notice_display: crate::session::config::NoticeDisplay,
    /// Resolved `acp.notice_timeout_secs`, used when `acp_notice_display` is `timeout`.
    pub acp_notice_timeout_secs: u32,
    /// `"debug"` when built with `debug_assertions`, else `"release"`. The web
    /// UI renders a DEV badge from it so concurrent debug (8081) and release
    /// (8080) instances are distinguishable, PWA installs included.
    pub build_flavor: &'static str,
    /// Content-hashed entry bundle name (`index-<hash>.js`) of the
    /// embedded dashboard build. The client compares this against its
    /// own entry script tag and offers a reload when they differ, so
    /// installed PWAs (which have no refresh affordance) pick up new
    /// dashboard code after the binary updates.
    pub web_build_id: Option<&'static str>,
    /// Read-only runtime state of the daemon's sleep-inhibit reconciler.
    pub sleep_inhibit: SleepInhibitStatus,
    /// This daemon run's id, which the web client sends back as a create's
    /// `retry_origin` so a restarted daemon never re-runs an attempt it cannot see.
    pub create_boot_id: String,
}

pub async fn get_about(State(state): State<Arc<AppState>>) -> Json<ServerAbout> {
    let auth_required = !state.token_manager.is_no_auth().await;
    let passphrase_enabled = state.login_manager.is_enabled();
    let auth_mode =
        crate::server::resolve_auth_mode(&state.token_manager, &state.login_manager).await;
    let _namespace = state.profile_namespace.read().await;
    let profile = state.served_profile().to_string();
    let acp_cfg = crate::session::config::profile_config::resolve_config_or_warn(&profile).acp;
    let acp_show_tool_durations = acp_cfg.show_tool_durations;
    let acp_wrap_tool_output = acp_cfg.wrap_tool_output;
    let acp_replay_events = acp_cfg.replay_events;
    let acp_compaction_reminder = acp_cfg.compaction_reminder;
    let acp_compaction_reminder_percent = acp_cfg.compaction_reminder_percent;
    let acp_notice_display = acp_cfg.notice_display;
    let acp_notice_timeout_secs = acp_cfg.notice_timeout_secs;
    let snapshot = state
        .sleep_inhibit_snapshot
        .load(std::sync::atomic::Ordering::Relaxed);
    let sleep_inhibit = derive_sleep_inhibit_status(
        snapshot & crate::server::SLEEP_INHIBIT_SNAPSHOT_ENABLED != 0,
        snapshot & crate::server::SLEEP_INHIBIT_SNAPSHOT_SLOT_PRESENT != 0,
        crate::process::sleep_inhibit_backend_available(),
    );
    Json(ServerAbout {
        version: env!("CARGO_PKG_VERSION").to_string(),
        auth_required,
        passphrase_enabled,
        auth_mode,
        read_only: state.read_only,
        behind_tunnel: state.behind_tunnel,
        cityhall_mode: state.cityhall_mode,
        profile,
        acp_show_tool_durations,
        acp_wrap_tool_output,
        acp_replay_events,
        acp_compaction_reminder,
        acp_compaction_reminder_percent,
        acp_notice_display,
        acp_notice_timeout_secs,
        build_flavor: if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        web_build_id: crate::server::web_build_id(),
        sleep_inhibit,
        create_boot_id: state.create_progress.boot_id().to_string(),
    })
}

// --- Update status ---

/// Web-facing snapshot of `update::check_for_update`. `update_check_mode`
/// mirrors `updates.update_check_mode` so the frontend can hide its banner
/// (mode = `off`) or skip nagging while a background install runs
/// (mode = `auto`) without separately fetching settings. See #984 and #1140.
#[derive(Serialize)]
pub struct UpdateStatusResponse {
    pub update_check_mode: crate::session::config::UpdateCheckMode,
    pub current_version: String,
    pub latest_version: Option<String>,
    pub update_available: bool,
    pub release_url: Option<String>,
    /// Set when the GitHub check failed (e.g. rate-limited, offline).
    /// Frontend keeps polling on its normal cadence; the banner stays
    /// hidden until a successful poll. The error is exposed so the
    /// settings UI can surface a one-liner if useful later.
    pub error: Option<String>,
    /// The version the user has already dismissed the update banner for,
    /// from the shared `app_state.dismissed_update_version`. Server-side so a
    /// dismissal sticks across devices and matches the TUI (#release-notes
    /// should only be acknowledged once, not per browser).
    pub dismissed_version: Option<String>,
    /// How the Daemon's own binary was installed, so the banner can say how to update it;
    /// `unknown` while update checks are off, since nothing is offered then.
    pub install_method: crate::update::install::InstallKind,
    /// What to run on the Daemon's machine to update.
    pub update_command: &'static str,
}

pub async fn get_update_status(State(state): State<Arc<AppState>>) -> Json<UpdateStatusResponse> {
    let namespace = state.profile_namespace.read().await;
    let profile = state.served_profile().to_string();
    let cfg = crate::session::config::profile_config::resolve_config_or_warn(&profile);
    drop(namespace);
    let current = env!("CARGO_PKG_VERSION").to_string();
    let mode = cfg.updates.update_check_mode;

    if !mode.is_enabled() {
        // No banner shows, so don't shell out to `brew` to detect the install method.
        let install_method = crate::update::install::InstallKind::Unknown;
        return Json(UpdateStatusResponse {
            update_check_mode: mode,
            current_version: current,
            latest_version: None,
            update_available: false,
            release_url: None,
            error: None,
            dismissed_version: cfg.app_state.dismissed_update_version.clone(),
            install_method,
            update_command: install_method.update_command(),
        });
    }

    let install_method = crate::update::install::install_kind().await;
    let update_command = install_method.update_command();

    match crate::update::check_for_update(&current, false).await {
        Ok(info) => {
            // Same rule as the TUI: hide an update `aoe update` can't install yet.
            let update_available = info.available
                && crate::update::install::update_actionable(&info.latest_version).await;
            let release_url = if info.latest_version.is_empty() {
                None
            } else {
                Some(crate::update::release_page_url(&info.latest_version))
            };
            Json(UpdateStatusResponse {
                update_check_mode: mode,
                current_version: info.current_version,
                latest_version: if info.latest_version.is_empty() {
                    None
                } else {
                    Some(info.latest_version)
                },
                update_available,
                release_url,
                error: None,
                dismissed_version: cfg.app_state.dismissed_update_version.clone(),
                install_method,
                update_command,
            })
        }
        Err(e) => Json(UpdateStatusResponse {
            update_check_mode: mode,
            current_version: current,
            latest_version: None,
            update_available: false,
            release_url: None,
            error: Some(e.to_string()),
            dismissed_version: cfg.app_state.dismissed_update_version.clone(),
            install_method,
            update_command,
        }),
    }
}

// --- Profile management ---

#[cfg(test)]
struct ProfileCommitPause {
    profile: String,
    entered: tokio::sync::oneshot::Sender<()>,
    resume: std::sync::mpsc::Receiver<()>,
}

#[cfg(test)]
static PROFILE_COMMIT_PAUSE: std::sync::Mutex<Option<ProfileCommitPause>> =
    std::sync::Mutex::new(None);

async fn commit_profile(
    state: Arc<AppState>,
    mutation: crate::daemon::ProfileMutation,
) -> axum::response::Response {
    use crate::daemon::ProfileMutation;

    let names = match &mutation {
        ProfileMutation::Create(body) => [Some(body.name.as_str()), None],
        ProfileMutation::Rename { name, body } => {
            [Some(name.as_str()), Some(body.new_name.as_str())]
        }
        ProfileMutation::Delete { name, query } => {
            [Some(name.as_str()), query.replacement_default.as_deref()]
        }
        ProfileMutation::SetDefault(body) => [Some(body.name.as_str()), None],
    };
    for name in names.into_iter().flatten() {
        if let Err(error) = validate_profile_name(name) {
            return api_error(StatusCode::BAD_REQUEST, "validation_failed", error);
        }
    }
    match state
        .runtime
        .work
        .run(
            "profile.catalogue",
            commit_profile_owned(state.clone(), mutation),
        )
        .await
    {
        Ok(response) => response,
        Err(crate::server::runtime::RuntimeWorkError::ShuttingDown) => {
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
        Err(crate::server::runtime::RuntimeWorkError::Interrupted) => {
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn commit_profile_owned(
    state: Arc<AppState>,
    mutation: crate::daemon::ProfileMutation,
) -> axum::response::Response {
    use crate::daemon::{ProfileMutation, ReloadFailureCode, RuntimeHealth};
    use crate::server::reload::{load_all_profiles, merge_loaded_rows};

    let transaction = tokio::select! {
        _ = state.shutdown.cancelled() => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
        transaction = crate::daemon::lifecycle::Transaction::acquire() => match transaction {
            Ok(transaction) => transaction,
            Err(error) => {
                tracing::warn!(target: "http.api.system", %error, "profile catalogue ownership unavailable");
                return (StatusCode::CONFLICT, crate::daemon::ApiErrorCode::LifecycleLocked.header(),
                    Json(serde_json::json!({"error": "lifecycle_busy", "message": "Daemon lifecycle is busy"}))).into_response();
            }
        },
    };
    let namespace = tokio::select! {
        _ = state.shutdown.cancelled() => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
        namespace = state.profile_namespace.write() => namespace,
    };
    if matches!(&mutation, ProfileMutation::Delete { name, .. } if name == &*state.served_profile())
    {
        return api_error(
            StatusCode::CONFLICT,
            "served_profile",
            "The profile served by this daemon cannot be deleted",
        );
    }
    if *state.canonical_health.read().await != RuntimeHealth::Healthy {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let creation_active = match &mutation {
        ProfileMutation::Rename { name, .. } | ProfileMutation::Delete { name, .. } => {
            state.session_service.has_profile_creation(name)
        }
        _ => false,
    };
    if creation_active {
        return (
            StatusCode::CONFLICT,
            crate::daemon::ApiErrorCode::LifecycleLocked.header(),
            Json(serde_json::json!({"error": "lifecycle_busy", "message": "Profile has an active creation"})),
        ).into_response();
    }
    let file_watch = state.file_watch.clone();
    let service = state.session_service.clone();
    let runtime = tokio::runtime::Handle::current();
    let prepared = tokio::task::spawn_blocking(move || {
        let identity = crate::session::acquire_session_identity_lock()?;
        let reload = runtime.block_on(service.disk_reload_guard());
        let catalogue = crate::session::ProfileCatalogueTransaction::with_locked_identity(
            transaction,
            identity,
        )?;
        let loaded = load_all_profiles(&file_watch).map(|_| ());
        Ok::<_, anyhow::Error>((catalogue, loaded, reload))
    })
    .await
    .map_err(anyhow::Error::from)
    .and_then(|result| result);
    let (catalogue, preflight, reload) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            tracing::warn!(target: "http.api.system", %error, "profile catalogue preparation failed");
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };
    if let Err(error) = preflight {
        state.mark_reload_failure(error.health).await;
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let publication = state.publication.write().await;
    if *state.canonical_health.read().await != RuntimeHealth::Healthy {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let file_watch = state.file_watch.clone();
    let committed = tokio::task::spawn_blocking(move || {
        let mut directory_moved = false;
        let result = match &mutation {
            ProfileMutation::Create(body) => catalogue.create(&body.name),
            ProfileMutation::Rename { name, body } => {
                let outcome = catalogue.rename(name, &body.new_name);
                directory_moved = outcome.directory_moved;
                outcome.result
            }
            ProfileMutation::Delete { name, query } => catalogue
                .delete(name, query.replacement_default.as_deref())
                .map(|_| ()),
            ProfileMutation::SetDefault(body) => catalogue.set_default(&body.name),
        };
        let loaded = load_all_profiles(&file_watch);
        #[cfg(test)]
        {
            let pause = {
                let mut slot = PROFILE_COMMIT_PAUSE.lock().unwrap();
                if slot.as_ref().is_some_and(|pause| matches!(&mutation,
                    ProfileMutation::Rename { name, .. } | ProfileMutation::Delete { name, .. } if name == &pause.profile)) {
                    slot.take()
                } else { None }
            };
            if let Some(pause) = pause {
                let _ = pause.entered.send(());
                pause.resume.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
            }
        }
        (catalogue, mutation, directory_moved, result, loaded, reload)
    })
    .await;
    let (catalogue, mutation, directory_moved, result, loaded, reload) = match committed {
        Ok(committed) => committed,
        Err(error) => {
            tracing::warn!(target: "http.api.system", %error, "profile catalogue commit task failed");
            *state.canonical_health.write().await = RuntimeHealth::Degraded {
                code: ReloadFailureCode::Metadata,
                profiles: Vec::new(),
            };
            state.runtime.request_publish();
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };
    if directory_moved {
        if let ProfileMutation::Rename { name, body } = &mutation {
            state.rename_served_profile(name, &body.new_name);
            crate::server::rename_profile_disk_watch(&state, name, &body.new_name).await;
        }
    }
    let loaded = match loaded {
        Ok(loaded) => loaded,
        Err(error) => {
            *state.canonical_health.write().await = error.health;
            state.runtime.request_publish();
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };
    let suppressed =
        crate::session::recovery::snapshot_recently_restarted(&state.recently_restarted);
    merge_loaded_rows(
        &mut *state.instances.write().await,
        loaded.instances,
        crate::server::state::StatusSource::DiskOnly,
        &suppressed,
    );
    *state.canonical_metadata.write().await = loaded.metadata;
    *state.canonical_health.write().await = RuntimeHealth::Healthy;
    state
        .mutation_epoch
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    state.runtime.request_publish();
    drop(publication);
    drop(reload);

    if result.is_ok() {
        match &mutation {
            ProfileMutation::Create(body) => {
                crate::server::add_profile_disk_watch(&state, &body.name).await
            }
            ProfileMutation::Rename { .. } => {}
            ProfileMutation::Delete { name, .. } => {
                crate::server::remove_profile_disk_watch(&state, name).await
            }
            ProfileMutation::SetDefault(_) => {}
        }
    }
    drop(namespace);
    drop(catalogue);
    let snapshot = match state.runtime.publish(&state).await {
        Ok(snapshot) => snapshot,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    if let Err(error) = result {
        if error.is::<crate::session::purge_owners::ProfileHasPendingPurges>() {
            return (
                StatusCode::CONFLICT,
                crate::daemon::ApiErrorCode::LifecycleLocked.header(),
                Json(serde_json::json!({"error": "pending_purge", "message": error.to_string()})),
            )
                .into_response();
        }
        let code = match mutation {
            ProfileMutation::Create(_) => "create_failed",
            ProfileMutation::Rename { .. } => "rename_failed",
            ProfileMutation::Delete { .. } => "delete_failed",
            ProfileMutation::SetDefault(_) => "update_failed",
        };
        return api_error(StatusCode::BAD_REQUEST, code, error.to_string());
    }
    let status = if matches!(mutation, ProfileMutation::Create(_)) {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    crate::server::runtime::mutation_response(
        &snapshot.value.cursor,
        (status, Json(serde_json::json!({"ok": true}))),
    )
}

pub async fn create_profile(
    State(state): State<Arc<AppState>>,
    body: Result<Json<crate::daemon::CreateProfileBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return super::read_only_response();
    }
    if let Some(response) = super::cityhall_block(&state) {
        return response;
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(error) => return error.into_response(),
    };
    commit_profile(state, crate::daemon::ProfileMutation::Create(body)).await
}

pub async fn delete_profile(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(name): axum::extract::Path<String>,
    query: Result<
        axum::extract::Query<crate::daemon::DeleteProfileQuery>,
        axum::extract::rejection::QueryRejection,
    >,
) -> impl IntoResponse {
    if state.read_only {
        return super::read_only_response();
    }
    if let Some(response) = super::cityhall_block(&state) {
        return response;
    }
    let axum::extract::Query(query) = match query {
        Ok(query) => query,
        Err(error) => return error.into_response(),
    };
    commit_profile(
        state,
        crate::daemon::ProfileMutation::Delete { name, query },
    )
    .await
}

pub async fn rename_profile(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(name): axum::extract::Path<String>,
    body: Result<Json<crate::daemon::RenameProfileBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return super::read_only_response();
    }
    if let Some(response) = super::cityhall_block(&state) {
        return response;
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(error) => return error.into_response(),
    };
    commit_profile(state, crate::daemon::ProfileMutation::Rename { name, body }).await
}

pub async fn default_profile(
    State(state): State<Arc<AppState>>,
    body: Result<Json<crate::daemon::DefaultProfileBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return super::read_only_response();
    }
    if let Some(response) = super::cityhall_block(&state) {
        return response;
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(error) => return error.into_response(),
    };
    commit_profile(state, crate::daemon::ProfileMutation::SetDefault(body)).await
}

pub async fn get_profile_settings(
    axum::extract::Path(name): axum::extract::Path<String>,
) -> impl IntoResponse {
    if let Err(e) = validate_profile_name(&name) {
        return api_error(StatusCode::BAD_REQUEST, "validation_failed", e);
    }
    let result = tokio::task::spawn_blocking(move || {
        let profile = crate::session::load_profile_config(&name)?;
        let global = crate::session::Config::load_or_warn();
        let mut val = serde_json::to_value(&profile)?;
        // `logging` lives on the global Config with no profile override
        // surface, so splice it in; otherwise the settings dropdowns reset on
        // every page load even after a successful PATCH.
        if let Some(obj) = val.as_object_mut() {
            obj.insert(
                "logging".to_string(),
                serde_json::to_value(&global.logging)?,
            );
            // Plugin settings are global-only at Tier 0, so splice them in or
            // the dashboard reverts to manifest defaults on every profile-view
            // load (#2094).
            obj.insert(
                "plugins".to_string(),
                serde_json::to_value(&global.plugins)?,
            );
        }
        Ok::<_, anyhow::Error>(val)
    })
    .await;
    match result {
        Ok(Ok(val)) => (StatusCode::OK, Json(val)).into_response(),
        Ok(Err(e)) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "load_failed",
            e.to_string(),
        ),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    }
}

/// Leaf paths CityHall mode may write via the profile-settings PATCH: only the
/// curated trash cluster the Sessions tab exposes. Everything else is closed so
/// the endpoint (which must stay open for those toggles) cannot double as an
/// arbitrary profile-override writer. See #7.
const CITYHALL_PROFILE_LEAVES: &[&str] = &[
    "session.delete_to_trash",
    "session.confirm_delete",
    "session.trash_retention_minutes",
];

/// Walk a sparse settings patch and return the first dotted leaf path not in
/// [`CITYHALL_PROFILE_LEAVES`], or `None` when every leaf is permitted.
fn first_non_cityhall_profile_leaf(patch: &serde_json::Value) -> Option<String> {
    fn walk(prefix: &str, v: &serde_json::Value) -> Option<String> {
        match v {
            serde_json::Value::Object(map) => {
                for (k, child) in map {
                    let path = if prefix.is_empty() {
                        k.clone()
                    } else {
                        format!("{prefix}.{k}")
                    };
                    if let Some(bad) = walk(&path, child) {
                        return Some(bad);
                    }
                }
                None
            }
            _ if CITYHALL_PROFILE_LEAVES.contains(&prefix) => None,
            _ => Some(prefix.to_string()),
        }
    }
    walk("", patch)
}

pub async fn update_profile_settings(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(name): axum::extract::Path<String>,
    headers: HeaderMap,
    session: Option<axum::Extension<AuthenticatedSession>>,
    loopback: Option<axum::Extension<Principal>>,
    body: Result<Json<serde_json::Value>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return super::read_only_response();
    }
    let Json(mut body) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };
    if let Err(e) = validate_profile_name(&name) {
        return api_error(StatusCode::BAD_REQUEST, "validation_failed", e);
    }
    // Keep the CityHall profile endpoint restricted to its explicit allowlist.

    if state.cityhall_mode {
        if let Some(bad) = first_non_cityhall_profile_leaf(&body) {
            return (
                StatusCode::FORBIDDEN,
                crate::daemon::ApiErrorCode::CityhallMode.header(),
                Json(serde_json::json!({
                    "error": "cityhall_mode",
                    "message": format!("Field '{bad}' is not writable in CityHall mode"),
                })),
            )
                .into_response();
        }
    }
    let elevated = handler_elevated(
        &state,
        session.as_deref(),
        authorize(loopback.as_deref(), Capability::SkipElevation),
    )
    .await;

    // Validate every leaf against the schema. Global-only fields are invalid here.
    // Elevation failures retain the payload consumed by the web prompt.
    if let Err(rej) = validate_patch(&body, Scope::Profile, elevated) {
        return reject_response(rej);
    }

    // Validate scope before stripping, including global-only local fields.
    strip_local_only(&mut body);

    let stamp = WriteStamp::from_headers(&headers);
    let mut applied = write_gate().await;
    if drop_stale_leaves(&applied, stamp.as_ref(), &name, &mut body) && is_empty_patch(&body) {
        return stale_write_response();
    }
    let scope = name.clone();
    let written = body.clone();
    let result = tokio::task::spawn_blocking(move || apply_profile_patch(&name, &written)).await;
    if matches!(&result, Ok(Ok(_))) {
        record_applied(&mut applied, stamp.as_ref(), &scope, &body);
    }
    drop(applied);

    match result {
        Ok(Ok(config)) => match serde_json::to_value(&config) {
            Ok(val) => (StatusCode::OK, Json(val)).into_response(),
            Err(e) => api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "serialize_failed",
                e.to_string(),
            ),
        },
        Ok(Err(e)) => api_error(StatusCode::BAD_REQUEST, "update_failed", e.to_string()),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    }
}

// --- Sounds ---

pub async fn list_sounds() -> Json<Vec<String>> {
    Json(crate::sound::list_available_sounds())
}

/// Serve a sound file by name so the acp's browser-side approval
/// player can fetch it from the same origin as the dashboard. The name
/// is validated against `list_available_sounds()` to block path
/// traversal: an attacker who can hit `/api/sounds/file/<x>` cannot
/// read arbitrary disk paths, only files already present in the user's
/// `sounds/` directory.
pub async fn serve_sound_file(
    axum::extract::Path(name): axum::extract::Path<String>,
) -> impl IntoResponse {
    // `list_available_sounds` does a sync `read_dir`, so validation stays on
    // the blocking pool; the file read uses `tokio::fs::read`.
    let lookup_name = name.clone();
    let validated = tokio::task::spawn_blocking(move || {
        if !crate::sound::list_available_sounds().contains(&lookup_name) {
            return None;
        }
        crate::sound::get_sounds_dir().map(|dir| dir.join(&lookup_name))
    })
    .await;

    let path = match validated {
        Ok(Some(p)) => p,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };

    let bytes = match tokio::fs::read(&path).await {
        Ok(b) => b,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };

    let content_type = match std::path::Path::new(&name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("ogg") => "audio/ogg",
        _ => "audio/wav",
    };

    (
        StatusCode::OK,
        [
            (axum::http::header::CONTENT_TYPE, content_type),
            (axum::http::header::CACHE_CONTROL, "private, max-age=3600"),
        ],
        bytes,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    /// A served profile deleted from disk lists no Groups, and listing doesn't recreate it.
    #[tokio::test]
    #[serial_test::serial]
    async fn list_groups_never_recreates_a_missing_profile() -> anyhow::Result<()> {
        use axum::response::IntoResponse;
        let _home = crate::session::test_support::isolate_app_dir();
        let state = crate::server::test_support::build_test_app_state(Vec::new());
        let served = state.served_profile().to_string();
        state.rename_served_profile(&served, "ghost");
        let response = super::list_groups(
            axum::extract::State(state),
            axum::extract::Query(super::GroupsQuery { profile: None }),
        )
        .await
        .into_response();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
        assert_eq!(body.as_ref(), b"[]");
        let ghost = crate::session::get_app_dir()?
            .join("profiles")
            .join("ghost");
        assert!(!ghost.exists(), "{}", ghost.display());
        Ok(())
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn list_groups_uses_canonical_profiles_and_counts_only_live_sessions(
    ) -> anyhow::Result<()> {
        use crate::daemon::ProfileSnapshot;
        use crate::session::{Group, Instance};

        let _home = crate::session::test_support::isolate_app_dir();
        let mut live = Instance::new("live", "/tmp");
        live.source_profile = "memory".into();
        live.group_path = "client/api".into();
        let mut archived = live.clone();
        archived.id = "archived".into();
        archived.archived_at = Some(chrono::Utc::now());
        let mut trashed = live.clone();
        trashed.id = "trashed".into();
        trashed.trashed_at = Some(chrono::Utc::now());
        let mut other = live.clone();
        other.id = "other".into();
        other.source_profile = "other".into();
        let state =
            crate::server::test_support::build_test_app_state(vec![live, archived, trashed, other]);
        let served = state.served_profile().to_string();
        state.rename_served_profile(&served, "memory");
        {
            let mut metadata = state.canonical_metadata.write().await;
            metadata.default_profile = "memory".into();
            metadata.profiles = vec![ProfileSnapshot {
                name: "memory".into(),
                description: None,
                groups: vec![Group::new("empty", "empty")],
                projects: Vec::new(),
            }];
        }
        let read = |profile: Option<&str>| {
            let state = state.clone();
            let query = GroupsQuery {
                profile: profile.map(str::to_owned),
            };
            async move {
                let response = list_groups(State(state), axum::extract::Query(query))
                    .await
                    .into_response();
                let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap();
                serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()
            }
        };
        let expected = serde_json::json!([
            {"path": "client", "session_count": 0, "archived": false},
            {"path": "client/api", "session_count": 1, "archived": false},
            {"path": "empty", "session_count": 0, "archived": false},
        ]);
        for query in [None, Some(""), Some("memory")] {
            assert_eq!(read(query).await, expected, "{query:?}");
        }
        assert_eq!(read(Some("missing")).await, serde_json::json!([]));
        state.rename_served_profile("memory", "");
        assert_eq!(read(None).await, expected, "canonical default profile");
        let root = crate::session::get_app_dir()?;
        assert!(
            !root.join("profiles/memory").exists(),
            "reads must not create a profile"
        );
        Ok(())
    }

    /// The wizard's Group list holds every group the sidebar can show: stored
    /// empty groups and the parents of nested paths, but not the synthetic shelves.
    #[test]
    fn group_infos_merge_the_tree_with_session_counts() {
        use crate::session::{
            Group, ARCHIVED_SECTION_PATH, SCRATCH_GROUP_PATH, TRASH_SECTION_PATH,
        };
        let mut archived = Group::new("old", "old");
        archived.archived_at = Some(chrono::Utc::now());
        let stored = [Group::new("empty", "empty"), archived];
        let sessions: Vec<String> = [
            "client/api",
            "client/api",
            "zeta",
            "",
            SCRATCH_GROUP_PATH,
            &format!("{ARCHIVED_SECTION_PATH}/repo"),
            TRASH_SECTION_PATH,
        ]
        .iter()
        .map(|p| p.to_string())
        .collect();
        let got: Vec<(String, usize, bool)> = group_infos(&sessions, &stored)
            .into_iter()
            .map(|g| (g.path, g.session_count, g.archived))
            .collect();
        assert_eq!(
            got,
            [
                ("client".to_string(), 0, false),
                ("client/api".to_string(), 2, false),
                ("empty".to_string(), 0, false),
                ("old".to_string(), 0, true),
                ("zeta".to_string(), 1, false),
            ]
        );
        assert!(group_infos(&[], &[]).is_empty());
    }

    use super::*;
    use axum::body::to_bytes;
    use std::collections::HashMap;

    #[tokio::test]
    #[serial_test::serial]
    async fn cancelled_catalogue_waiter_cannot_abandon_rename_adoption() -> anyhow::Result<()> {
        let _home = crate::session::test_support::isolate_app_dir();
        let root = crate::session::get_app_dir()?;
        crate::session::purge_owners::initialize(&root)?;
        let storage = crate::session::Storage::new_unwatched("source")?;
        let mut row = crate::session::Instance::new("rename producer", "/tmp");
        row.source_profile = "source".into();
        row.view = crate::session::View::Structured;
        let id = row.id.clone();
        storage.update(|rows, _| {
            rows.push(row.clone());
            Ok(())
        })?;
        let state = crate::server::test_support::build_test_app_state(vec![row]);
        let served_profile = state.served_profile().to_string();
        state.rename_served_profile(&served_profile, "source");
        assert_eq!(&*state.served_profile(), "source");
        crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
        let epoch = state
            .mutation_epoch
            .load(std::sync::atomic::Ordering::SeqCst);
        let (entered, committed) = tokio::sync::oneshot::channel();
        let (resume, paused) = std::sync::mpsc::channel();
        *PROFILE_COMMIT_PAUSE.lock().unwrap() = Some(ProfileCommitPause {
            profile: "source".into(),
            entered,
            resume: paused,
        });
        let requester = tokio::spawn(commit_profile(
            state.clone(),
            crate::daemon::ProfileMutation::Rename {
                name: "source".into(),
                body: crate::daemon::RenameProfileBody {
                    new_name: "renamed".into(),
                },
            },
        ));
        committed.await?;
        assert!(root.join("profiles/renamed").is_dir());
        assert!(!root.join("profiles/source").exists());
        requester.abort();
        assert!(requester.await.unwrap_err().is_cancelled());
        let enqueue = state.session_service.enqueue_prompt(
            &id,
            "after-rename".into(),
            "accepted".into(),
            vec![],
            None,
            None,
        );
        tokio::pin!(enqueue);
        assert!(futures_util::poll!(&mut enqueue).is_pending());
        resume.send(())?;
        assert_eq!(enqueue.await.unwrap().seq, 0);
        assert_eq!(&*state.served_profile(), "renamed");
        assert!(
            state
                .mutation_epoch
                .load(std::sync::atomic::Ordering::SeqCst)
                > epoch
        );
        assert!(
            !root.join("profiles/source").exists(),
            "queue must not recreate the old catalogue"
        );
        let rows = crate::session::Storage::open_unwatched("renamed")?.load()?;
        assert_eq!(rows[0].queued_prompts[0].id, "after-rename");
        assert_eq!(state.instances.read().await[0].source_profile, "renamed");
        state.runtime.work.drain().await;
        Ok(())
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn retained_purge_owner_refuses_profile_mutation_without_degrading_health(
    ) -> anyhow::Result<()> {
        let _home = crate::session::test_support::isolate_app_dir();
        let root = crate::session::get_app_dir()?;
        crate::session::purge_owners::initialize(&root)?;
        let storage = crate::session::Storage::new_unwatched("source")?;
        crate::session::Storage::new_unwatched("keep")?;
        let mut row = crate::session::Instance::new("retained owner", "/tmp");
        row.source_profile = "source".into();
        row.terminal_scopes = Some(vec![]);
        let owner = crate::session::purge_owners::PurgeOwner::record(
            &storage,
            &row,
            crate::session::purge_owners::PurgeCapture::new(&row)?,
        )?;
        let state = crate::server::test_support::build_test_app_state(vec![]);
        crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
        for mutation in [
            crate::daemon::ProfileMutation::Rename {
                name: "source".into(),
                body: crate::daemon::RenameProfileBody {
                    new_name: "renamed".into(),
                },
            },
            crate::daemon::ProfileMutation::Delete {
                name: "source".into(),
                query: crate::daemon::DeleteProfileQuery {
                    replacement_default: Some("keep".into()),
                },
            },
        ] {
            let response = commit_profile(state.clone(), mutation).await;
            assert_eq!(response.status(), StatusCode::CONFLICT);
            assert_eq!(
                response.headers()[crate::daemon::ERROR_CODE_HEADER],
                "lifecycle_locked"
            );
            let body: serde_json::Value =
                serde_json::from_slice(&to_bytes(response.into_body(), 65536).await?)?;
            assert_eq!(body["error"], "pending_purge");
            assert_eq!(
                *state.canonical_health.read().await,
                crate::daemon::RuntimeHealth::Healthy
            );
            assert!(root.join("profiles/source").is_dir());
            assert!(!root.join("profiles/renamed").exists());
        }
        owner.release()?;
        let response = commit_profile(
            state,
            crate::daemon::ProfileMutation::Rename {
                name: "source".into(),
                body: crate::daemon::RenameProfileBody {
                    new_name: "renamed".into(),
                },
            },
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        Ok(())
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn profile_catalogue_mutations_publish_before_receipt() -> anyhow::Result<()> {
        use axum::{
            body::Body,
            http::Request,
            routing::{delete, get, patch},
            Router,
        };
        use tower::ServiceExt;

        let temp = tempfile::tempdir()?;
        let _guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        crate::session::purge_owners::initialize(&crate::session::get_app_dir()?)?;
        let state = crate::server::test_support::build_test_app_state(Vec::new());
        crate::session::Storage::new("alpha", state.file_watch.clone())?;
        crate::session::save_profile_config(
            "alpha",
            &crate::session::ProfileConfig {
                description: Some("retained catalogue description".into()),
                ..Default::default()
            },
        )?;
        crate::session::set_default_profile("alpha")?;

        *state.canonical_metadata.write().await =
            crate::server::reload::load_all_profiles(&state.file_watch)?.metadata;
        let mut revision = state.runtime.publish(&state).await?.value.cursor.revision;
        let router = Router::new()
            .route("/api/profiles", get(list_profiles).post(create_profile))
            .route("/api/profiles/{name}", delete(delete_profile))
            .route("/api/profiles/{name}/rename", patch(rename_profile))
            .route("/api/default-profile", patch(default_profile))
            .with_state(state.clone());

        for (method, uri, body, status, expected_default, expected_names) in [
            (
                "POST",
                "/api/profiles",
                r#"{"name":"beta"}"#,
                StatusCode::CREATED,
                "alpha",
                &["alpha", "beta"][..],
            ),
            (
                "PATCH",
                "/api/default-profile",
                r#"{"name":"beta"}"#,
                StatusCode::OK,
                "beta",
                &["alpha", "beta"][..],
            ),
            (
                "PATCH",
                "/api/profiles/beta/rename",
                r#"{"new_name":"gamma"}"#,
                StatusCode::OK,
                "gamma",
                &["alpha", "gamma"][..],
            ),
            (
                "DELETE",
                "/api/profiles/gamma?replacement_default=alpha",
                "",
                StatusCode::OK,
                "alpha",
                &["alpha"][..],
            ),
        ] {
            if method == "DELETE" {
                for replacement in ["gamma", "missing"] {
                    let response = router
                        .clone()
                        .oneshot(
                            Request::builder()
                                .method("DELETE")
                                .uri(format!(
                                    "/api/profiles/gamma?replacement_default={replacement}"
                                ))
                                .body(Body::empty())?,
                        )
                        .await?;
                    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
                    assert!(crate::session::get_profile_dir_path("gamma")?.is_dir());
                    assert_eq!(crate::session::Config::load()?.default_profile, "gamma");
                }
            }
            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(uri)
                        .header("content-type", "application/json")
                        .body(Body::from(body))?,
                )
                .await?;
            assert_eq!(response.status(), status);
            let receipt = response
                .headers()
                .get(crate::daemon::RUNTIME_REVISION_HEADER)
                .expect("a successful profile mutation must acknowledge its published snapshot")
                .to_str()?
                .parse::<u64>()?;
            assert!(receipt > revision);
            revision = receipt;
            let snapshot = state.runtime.snapshot(&state).await?;
            assert_eq!(snapshot.value.cursor.revision, receipt);
            assert_eq!(snapshot.value.contents.default_profile, expected_default);
            assert_eq!(
                snapshot
                    .value
                    .contents
                    .profiles
                    .iter()
                    .map(|profile| profile.name.as_str())
                    .collect::<Vec<_>>(),
                expected_names
            );

            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/api/profiles")
                        .body(Body::empty())?,
                )
                .await?;
            assert_eq!(response.status(), StatusCode::OK);
            let profiles: Vec<serde_json::Value> =
                serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await?)?;
            assert_eq!(
                profiles
                    .iter()
                    .filter(|profile| profile["is_default"] == true)
                    .map(|profile| profile["name"].as_str().unwrap())
                    .collect::<Vec<_>>(),
                [expected_default]
            );
        }
        let served_profile = state.served_profile().to_string();
        state.rename_served_profile(&served_profile, "alpha");
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/profiles/alpha")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert!(crate::session::get_profile_dir_path("alpha")?.is_dir());
        assert_eq!(&*state.served_profile(), "alpha");
        assert_eq!(
            state.runtime.snapshot(&state).await?.value.cursor.revision,
            revision
        );
        std::fs::write(
            crate::session::get_profile_dir_path("alpha")?.join("config.toml"),
            "description = [",
        )?;
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/profiles")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"name":"not-committed"}"#))?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(!crate::session::get_profile_dir_path("not-committed")?.exists());
        let snapshot = state.runtime.publish(&state).await?;
        assert!(
            matches!(&snapshot.value.contents.health, crate::daemon::RuntimeHealth::Degraded {
            code: crate::daemon::ReloadFailureCode::ProfileData, profiles
        } if profiles == &["alpha"])
        );
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/profiles")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        let profiles: Vec<serde_json::Value> =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await?)?;
        assert_eq!(profiles[0]["description"], "retained catalogue description");
        Ok(())
    }

    #[test]
    fn derive_sleep_inhibit_status_gates_held_on_backend() {
        // The last two rows are unreachable from the writer but pin the pure
        // gate, proving `currently_held` excludes prevent_sleep_enabled.
        // (prevent_sleep_enabled, slot_present, backend_available) -> currently_held
        let cases = [
            // supported host actively holding the assertion
            ((true, true, true), true),
            // toggle on but every session idle past grace: slot released
            ((true, false, true), false),
            // backend latched unavailable or no-op platform: the slot is kept
            // to suppress respawns, yet no real assertion is held
            ((true, true, false), false),
            // gate guard: enabled must not force held when the backend is down
            ((false, true, false), false),
            // gate guard: held tracks slot AND backend, never prevent_sleep_enabled
            ((false, true, true), true),
        ];
        for ((enabled, slot, avail), held) in cases {
            let s = derive_sleep_inhibit_status(enabled, slot, avail);
            assert_eq!(
                s.currently_held, held,
                "enabled={enabled} slot={slot} avail={avail}"
            );
            assert_eq!(s.prevent_sleep_enabled, enabled);
            assert_eq!(s.backend_available, avail);
        }
    }

    #[test]
    fn cityhall_profile_leaf_allows_only_the_curated_trash_cluster() {
        // Every curated leaf, on its own and bundled, is permitted.
        assert_eq!(
            first_non_cityhall_profile_leaf(&serde_json::json!({
                "session": {
                    "delete_to_trash": true,
                    "confirm_delete": false,
                    "trash_retention_minutes": 43200
                }
            })),
            None
        );
        // An empty patch is a no-op, not a violation.
        assert_eq!(
            first_non_cityhall_profile_leaf(&serde_json::json!({"session": {}})),
            None
        );
        // Any leaf outside the allowlist is reported by its dotted path.
        assert_eq!(
            first_non_cityhall_profile_leaf(&serde_json::json!({
                "session": {"delete_to_trash": true, "yolo_mode": true}
            })),
            Some("session.yolo_mode".to_string())
        );
        assert_eq!(
            first_non_cityhall_profile_leaf(&serde_json::json!({"theme": {"name": "x"}})),
            Some("theme.name".to_string())
        );
    }

    #[test]
    fn skip_git_probe_avoids_protected_home_folders() {
        let home = std::path::Path::new("/Users/alice");

        // The TCC prompt is macOS-only, so other platforms probe normally.
        let macos = cfg!(target_os = "macos");

        // Protected folders directly under $HOME: skipped on macOS.
        for name in ["Downloads", "Desktop", "Music", "Pictures", "Documents"] {
            assert_eq!(
                skip_git_probe(home, name, Some(home)),
                macos,
                "unexpected probe decision for ~/{name}"
            );
        }

        // A real project directly under $HOME still gets probed, so its git
        // badge is preserved.
        assert!(!skip_git_probe(home, "myproject", Some(home)));

        // A system-looking name that is not directly under $HOME is probed.
        let sub = std::path::Path::new("/Users/alice/code");
        assert!(!skip_git_probe(sub, "Downloads", Some(home)));

        // No home resolved: never skip (falls back to normal probing).
        assert!(!skip_git_probe(home, "Downloads", None));
    }

    fn custom_agents(entries: &[(&str, &str)]) -> HashMap<String, String> {
        entries
            .iter()
            .map(|(name, command)| ((*name).to_string(), (*command).to_string()))
            .collect()
    }

    /// The default policy, for the tests that predate the allowlist and care
    /// about other fields.
    fn unrestricted() -> crate::acp::agent_policy::AgentPolicy {
        crate::acp::agent_policy::AgentPolicy::for_test(false, &[])
    }

    /// #3241: policy is a separate axis from capability. A disallowed agent must
    /// still report `acp_capable: true` so the settings surfaces that enumerate
    /// this endpoint can keep editing its per-agent structured-view defaults;
    /// only `acp_allowed` goes false. Overloading `acp_capable` would have hidden
    /// it from the operator's own settings UI.
    #[test]
    fn acp_allowed_is_independent_of_acp_capable() {
        let custom = custom_agents(&[("oc-sp", "ocp run sp"), ("blocked", "ssh host claude")]);
        let acp = custom_agents(&[
            ("oc-sp", "ocp run sp acp"),
            ("blocked", "ocp run blocked acp"),
        ]);
        let policy = crate::acp::agent_policy::AgentPolicy::for_test(true, &["oc-sp"]);
        let entries = build_custom_agent_infos(&custom, &acp, &HashMap::new(), &policy);

        let oc_sp = entries.iter().find(|e| e.name == "oc-sp").unwrap();
        assert!(oc_sp.acp_capable && oc_sp.acp_allowed);

        let blocked = entries.iter().find(|e| e.name == "blocked").unwrap();
        assert!(
            blocked.acp_capable,
            "capability is intrinsic and policy must not erase it"
        );
        assert!(!blocked.acp_allowed, "policy denies this agent");

        // Unrestricted leaves both true, so the default path is unchanged.
        let entries = build_custom_agent_infos(&custom, &acp, &HashMap::new(), &unrestricted());
        assert!(entries.iter().all(|e| e.acp_capable && e.acp_allowed));
    }

    #[test]
    fn custom_agent_entries_never_serialize_command_values() {
        let entries = build_custom_agent_infos(
            &custom_agents(&[("remote-agent", "ssh -t prod.example claude")]),
            &HashMap::new(),
            &HashMap::new(),
            &unrestricted(),
        );

        let json = serde_json::to_string(&entries).unwrap();
        assert!(json.contains("remote-agent"));
        assert!(!json.contains("ssh"));
        assert!(!json.contains("prod.example"));
        assert!(!json.contains("claude"));
    }

    #[test]
    fn serialized_custom_agent_response_contains_no_command_or_detect_as_data() {
        let entries = build_custom_agent_infos(
            &custom_agents(&[("remote-agent", "ssh -t prod.example claude")]),
            &HashMap::new(),
            &HashMap::new(),
            &unrestricted(),
        );
        let value = serde_json::to_value(&entries).unwrap();

        assert_eq!(value[0]["kind"], "custom");
        assert_eq!(value[0]["name"], "remote-agent");
        assert_eq!(value[0]["binary"], "remote-agent");
        assert_eq!(value[0]["installed"], true);
        assert_eq!(value[0]["host_only"], false);
        assert_eq!(value[0]["install_hint"], "Configured custom agent");

        let serialized = value.to_string();
        assert!(!serialized.contains("ssh -t prod.example claude"));
        assert!(!serialized.contains("prod.example"));
        assert!(!serialized.contains("agent_detect_as"));
    }

    #[test]
    fn custom_agent_entries_filter_empty_and_builtin_names_and_sort() {
        let entries = build_custom_agent_infos(
            &custom_agents(&[
                ("", "codex"),
                ("empty-command", ""),
                ("   ", "codex"),
                ("whitespace-command", "   "),
                ("claude", "ssh -t prod.example claude"),
                ("zeta", "zeta-cmd"),
                ("remote-codex", "ssh -t prod.example codex"),
                ("alpha", "alpha-cmd"),
            ]),
            &HashMap::new(),
            &HashMap::new(),
            &unrestricted(),
        );

        let names: Vec<_> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["alpha", "remote-codex", "zeta"],
            "sorted by name"
        );
        assert!(entries.iter().all(|entry| entry.kind == "custom"));
    }

    #[test]
    fn custom_agent_acp_capable_tracks_acp_cmd_and_detect_as() {
        let custom = custom_agents(&[("oc-sp", "ocp run sp"), ("plain", "ssh host claude")]);
        let acp = custom_agents(&[
            ("oc-sp", "ocp run sp acp"),
            // An entry whose command is malformed must not flip capability on.
            ("broken", "ocp run \"unterminated"),
        ]);
        let entries = build_custom_agent_infos(&custom, &acp, &HashMap::new(), &unrestricted());

        let oc_sp = entries.iter().find(|e| e.name == "oc-sp").unwrap();
        assert!(oc_sp.acp_capable, "agent with a valid acp cmd is capable");
        let plain = entries.iter().find(|e| e.name == "plain").unwrap();
        assert!(!plain.acp_capable, "agent with no acp cmd is tmux-only");
    }

    #[test]
    fn custom_agent_acp_capable_via_detect_as_inheritance() {
        // A wrapper that inherits a registry-backed base (claude) is
        // structured-capable through the base adapter, with no agent_acp_cmd.
        // A wrapper inheriting a terminal-only base (cursor) stays tmux-only.
        let custom = custom_agents(&[
            ("lenovo-claude", "CLAUDE_CONFIG_DIR=/work claude"),
            ("my-cursor", "agent"),
        ]);
        let detect_as = custom_agents(&[("lenovo-claude", "claude"), ("my-cursor", "cursor")]);
        let entries =
            build_custom_agent_infos(&custom, &HashMap::new(), &detect_as, &unrestricted());

        let lenovo = entries.iter().find(|e| e.name == "lenovo-claude").unwrap();
        assert!(
            lenovo.acp_capable,
            "wrapper inheriting claude is acp-capable via the base adapter"
        );
        let cursor = entries.iter().find(|e| e.name == "my-cursor").unwrap();
        assert!(
            !cursor.acp_capable,
            "wrapper inheriting a terminal-only base stays tmux-only"
        );
    }

    /// `acp_installed` follows the structured create check, so a custom
    /// agent counts when its adapter is on the configured `environment` PATH.
    #[tokio::test]
    #[serial_test::serial]
    async fn acp_installed_matches_the_structured_create_check() {
        use std::os::unix::fs::PermissionsExt;
        let _home = crate::session::test_support::isolate_app_dir();
        let bin = tempfile::tempdir().unwrap();
        let adapter = format!("aoe-test-adapter-{}", std::process::id());
        std::fs::write(bin.path().join(&adapter), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(
            bin.path().join(&adapter),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        crate::session::config::update_config(|config| {
            for (name, acp) in [
                ("on-path", Some(format!("{adapter} acp"))),
                ("missing", Some(format!("{adapter}-missing acp"))),
                ("terminal", None),
            ] {
                config
                    .session
                    .custom_agents
                    .insert(name.into(), "true".into());
                if let Some(acp) = acp {
                    config.session.agent_acp_cmd.insert(name.into(), acp);
                }
            }
            config.environment = vec![format!("PATH={}", bin.path().display())];
        })
        .unwrap();
        let state = crate::server::test_support::build_test_app_state(Vec::new());
        let Json(agents) = list_agents(State(state)).await;
        for (name, installed) in [("on-path", true), ("missing", false), ("terminal", false)] {
            let agent = agents.iter().find(|a| a.name == name).unwrap();
            assert_eq!(agent.acp_installed, installed, "{name}");
            assert_eq!(agent.display_name, name);
        }
        let claude = agents.iter().find(|a| a.name == "claude").unwrap();
        assert_eq!(claude.display_name, "Claude Code");
    }

    #[test]
    fn acp_command_fields_substitute_data_dir() {
        let dir = std::path::Path::new("/tmp/aoe-data");
        let spec = crate::acp::AgentSpec {
            command: "${aoe_data_dir}/bin/custom-acp".into(),
            args: vec![],
            description: "custom".into(),
            env_allowlist: None,
        };
        let (cmd, _) = acp_command_fields(Some(&spec), Some(dir));
        let cmd = cmd.expect("spec has a command");
        assert!(
            !cmd.contains("${aoe_data_dir}"),
            "placeholder must be substituted"
        );
        assert!(cmd.starts_with("/tmp/aoe-data/"));

        // The bundled agent resolves through the adapter install, so its
        // command is the bare binary token, not a data-dir path (#3553).
        let registry = crate::acp::AgentRegistry::with_defaults();
        let (cmd, _) = acp_command_fields(registry.get("aoe-agent"), Some(dir));
        assert_eq!(cmd.as_deref(), Some("aoe-agent"));
    }

    #[test]
    fn custom_agent_entries_omit_acp_command_fields() {
        let entries = build_custom_agent_infos(
            &custom_agents(&[("oc-sp", "ocp run sp")]),
            &custom_agents(&[("oc-sp", "ocp run sp acp")]),
            &HashMap::new(),
            &unrestricted(),
        );
        let value = serde_json::to_value(&entries).unwrap();
        assert!(value[0].get("acp_command").is_none());
        assert!(value[0].get("acp_args").is_none());
        // The custom acp command must not leak via the new fields.
        assert!(!value.to_string().contains("ocp run sp"));
    }

    /// Global-only systemd leaves save machine-wide; invalid leaves write nothing.
    #[tokio::test]
    #[serial_test::serial]
    async fn systemd_settings_save_machine_wide_and_reject_invalid_leaves() {
        use serde_json::json;
        let home = tempfile::TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_home(home.path());
        let state = crate::server::test_support::build_test_app_state(Vec::new());
        let save = |body: serde_json::Value| {
            let state = state.clone();
            async move {
                let response = update_settings(
                    State(state),
                    axum::extract::Query(SettingsQuery {
                        profile: None,
                        layer: None,
                    }),
                    HeaderMap::new(),
                    None,
                    Some(axum::Extension(Principal::LocalPeer { uid: 0 })),
                    Ok(Json(body)),
                )
                .await
                .into_response();
                let status = response.status();
                let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
                (status, String::from_utf8_lossy(&body).into_owned())
            }
        };
        let (status, _) = save(json!({"systemd": {
            "enabled": false, "slice": "aoe-{profile}.slice", "memory_max": "4G", "tasks_max": 512,
        }}))
        .await;
        assert_eq!(status, StatusCode::OK);
        let saved = crate::session::Config::load().unwrap().systemd;
        assert!(!saved.enabled);
        assert_eq!(saved.slice, "aoe-{profile}.slice");
        assert_eq!(saved.memory_max.as_deref(), Some("4G"));
        assert_eq!(saved.tasks_max, 512);
        assert!(
            saved.survive_daemon_restart,
            "untouched leaves keep their value"
        );
        let profile = crate::session::load_profile_config("test").unwrap();
        assert!(serde_json::to_value(profile)
            .unwrap()
            .get("systemd")
            .is_none());
        for (field, value) in [
            ("slice", json!("aoe.service")),
            ("memory_max", json!("4GB")),
        ] {
            let (status, body) = save(json!({"systemd": {field: value}})).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{field}");
            assert!(
                body.contains(&format!("systemd.{field}")),
                "{field}: {body}"
            );
        }
        let after = crate::session::Config::load().unwrap().systemd;
        assert_eq!(after.slice, "aoe-{profile}.slice");
        assert_eq!(after.memory_max.as_deref(), Some("4G"));
    }

    #[tokio::test]
    async fn serve_sound_file_rejects_unknown_name() {
        let resp = serve_sound_file(axum::extract::Path("does-not-exist-xyz.wav".to_string()))
            .await
            .into_response();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn serve_sound_file_rejects_path_traversal() {
        let resp = serve_sound_file(axum::extract::Path("../../../etc/passwd".to_string()))
            .await
            .into_response();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        // A NOT_FOUND that somehow still streamed a body would be a
        // worse failure than the wrong status, so assert the body is
        // empty rather than just "not /etc/passwd".
        let body = to_bytes(resp.into_body(), 1024).await.unwrap();
        assert!(body.is_empty(), "unexpected body bytes: {body:?}");
    }
    /// A PATCH whose connection dropped can commit after the client's newer edit of the
    /// same leaf; the stamp makes the server drop it instead of restoring the old value.
    #[tokio::test]
    #[serial_test::serial]
    async fn settings_save_drops_a_late_older_write_of_the_same_leaf() {
        use serde_json::json;
        let home = tempfile::TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_home(home.path());
        let state = crate::server::test_support::build_test_app_state(Vec::new());
        let save = |tab_seq: &'static str, slice: &'static str| {
            let state = state.clone();
            async move {
                let mut headers = HeaderMap::new();
                headers.insert(WRITE_STAMP_HEADER, tab_seq.parse().unwrap());
                update_settings(
                    State(state),
                    axum::extract::Query(SettingsQuery {
                        profile: None,
                        layer: Some("machine".into()),
                    }),
                    headers,
                    None,
                    None,
                    Ok(Json(json!({"systemd": {"slice": slice}}))),
                )
                .await
                .into_response()
                .status()
            }
        };

        assert_eq!(save("tab1.2", "newer.slice").await, StatusCode::OK);
        assert_eq!(save("tab1.1", "older.slice").await, StatusCode::CONFLICT);
        assert_eq!(
            crate::session::Config::load().unwrap().systemd.slice,
            "newer.slice"
        );
        // Another tab has its own sequence, and an unstamped write is never dropped.
        assert_eq!(save("tab2.1", "other-tab.slice").await, StatusCode::OK);
        assert_eq!(
            update_settings(
                State(state.clone()),
                axum::extract::Query(SettingsQuery {
                    profile: None,
                    layer: Some("machine".into()),
                }),
                HeaderMap::new(),
                None,
                None,
                Ok(Json(json!({"systemd": {"slice": "unstamped.slice"}}))),
            )
            .await
            .into_response()
            .status(),
            StatusCode::OK
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn theme_save_drops_a_late_older_write_of_the_same_leaf() {
        use crate::session::config::ColorMode;
        let home = tempfile::TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_home(home.path());
        let state = crate::server::test_support::build_test_app_state(Vec::new());
        let save = |tab_seq: &'static str, name: Option<String>, mode: Option<ColorMode>| {
            let state = state.clone();
            async move {
                let mut headers = HeaderMap::new();
                headers.insert(WRITE_STAMP_HEADER, tab_seq.parse().unwrap());
                update_theme(
                    State(state),
                    headers,
                    Ok(Json(ThemePatch {
                        name,
                        color_mode: mode,
                    })),
                )
                .await
                .into_response()
                .status()
            }
        };

        assert_eq!(
            save("tab1.2", None, Some(ColorMode::Palette)).await,
            StatusCode::OK
        );
        assert_eq!(
            save("tab1.1", None, Some(ColorMode::Truecolor)).await,
            StatusCode::CONFLICT
        );
        assert_eq!(
            crate::session::Config::load().unwrap().theme.color_mode,
            ColorMode::Palette
        );

        // A fresh name still lands when its stale color mode is dropped from the same patch.
        let name = crate::tui::styles::available_themes().remove(0);
        assert_eq!(
            save("tab1.1", Some(name.clone()), Some(ColorMode::Truecolor)).await,
            StatusCode::OK
        );
        let theme = crate::session::Config::load().unwrap().theme;
        assert_eq!(theme.name, name);
        assert_eq!(theme.color_mode, ColorMode::Palette);
    }
}
