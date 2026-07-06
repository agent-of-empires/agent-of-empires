//! The capability-gated host API a plugin worker calls over the worker
//! protocol.
//!
//! Every method maps to a capability the plugin must have declared in its
//! manifest and had granted at install. The middleware
//! (`PluginRpcContext::require`) refuses an undeclared or ungranted call
//! before the method runs, so a worker can never reach a resource it was not
//! approved for. This is the cooperative-plugin boundary of the honest v1
//! model (D8): it stops a well-behaved plugin from overreaching; it does not
//! contain an adversarial one (that needs the OS-level sandbox backends that
//! land later behind [`crate::plugin::sandbox::SandboxBackend`]).
//!
//! v1 method list:
//! - `events.publish { topic, payload }` and
//!   `events.subscribe { topics, after_seq }` over a shared plugin event bus
//!   (capability `runtime.worker`, which every worker holds to run at all).
//! - `session.meta.get { session_id, key }` (`session.read`).
//! - `session.meta.set { session_id, key, value }` and
//!   `session.meta.cas { session_id, key, expected, value }` (`session.write`).
//! - `sessions.list` (`session.read`).
//! - `config.get { key }` (`runtime.worker`): the value at
//!   `plugins.<plugin-id>.settings.<key>` for the calling plugin's own id.
//!
//! Per-plugin namespace: session metadata is always read and written under the
//! calling plugin's own `Instance.plugin_meta[<plugin-id>]` slot, and
//! `config.get` reads only the caller's own `plugins.<plugin-id>.settings`
//! table. The worker sends only `key`; it can never name another plugin's id,
//! so one plugin cannot touch another's metadata or settings. Reading one's own
//! declared settings needs no `config.*` capability (those gate host/global or
//! other-plugin config); `runtime.worker`, which every worker holds to run at
//! all, is enough.

use std::collections::HashSet;
use std::sync::Mutex;

use aoe_plugin_api::UiSlot;
use rusqlite::Connection;
use serde_json::{json, Value};

use crate::events::{self, Order, Schema, SeqBound};
use crate::plugin::protocol::codes;
use crate::plugin::ui_state::{Tone, UiError, UiSnapshot, UiStore};
use crate::session::Storage;

/// Capability required by each host method. Reused from the manifest taxonomy
/// (`aoe_plugin_api::KNOWN_CAPABILITIES`); no new capability is introduced.
const CAP_WORKER: &str = "runtime.worker";
const CAP_SESSION_READ: &str = "session.read";
const CAP_SESSION_WRITE: &str = "session.write";
/// `ui.notify` posts a notification; it reuses the existing `notifications`
/// capability. `ui.state.*` need no extra capability beyond `runtime.worker`:
/// the gate is the manifest `ui` slot declaration (see [`PluginRpcContext`]).
const CAP_NOTIFICATIONS: &str = "notifications";

/// Shared, host-owned state behind the API: the plugin event bus and the
/// profile whose session storage the API reads and writes. One per running
/// host; cloned cheaply via `Arc` by each worker's dispatch task.
pub struct HostApiState {
    events: Mutex<Connection>,
    schema: Schema,
    /// How many events to keep per topic before the oldest are pruned.
    retention: usize,
    /// Session-storage profile the API operates on (the daemon's profile).
    profile: String,
    /// Host-rendered UI state pushed by workers over `ui.state.*`/`ui.notify`.
    ui: UiStore,
}

impl HostApiState {
    /// Open (or create) the plugin event-bus database at `db_path` and bind the
    /// API to `profile`'s session storage.
    pub fn open(
        db_path: &std::path::Path,
        profile: &str,
        retention: usize,
    ) -> anyhow::Result<Self> {
        let schema = Schema::new("plugin_host")?;
        let conn = events::open(db_path, &schema)?;
        Ok(Self {
            events: Mutex::new(conn),
            schema,
            retention,
            profile: profile.to_string(),
            ui: UiStore::new(),
        })
    }

    fn storage(&self) -> anyhow::Result<Storage> {
        Storage::new_unwatched(&self.profile)
    }

    /// Register a freshly spawned worker's UI generation. The supervisor threads
    /// the returned value into the worker's [`PluginRpcContext`].
    pub fn begin_ui_generation(&self, plugin_id: &str) -> u64 {
        self.ui.begin_generation(plugin_id)
    }

    /// Clear a worker's UI entries when it exits, guarded by its generation.
    pub fn clear_ui(&self, plugin_id: &str, generation: u64) -> bool {
        self.ui.clear_plugin(plugin_id, generation)
    }

    /// The full UI-state snapshot the web dashboard renders.
    pub fn ui_snapshot(&self) -> UiSnapshot {
        self.ui.snapshot()
    }

    /// The UI mutation counter for one `(plugin, session)` scope (0 if none yet).
    pub fn ui_revision(&self, plugin_id: &str, session_id: Option<&str>) -> u64 {
        self.ui.revision(plugin_id, session_id)
    }

    /// Push a host-originated notification onto the ring. Unlike the `ui.notify`
    /// RPC this is the host itself speaking (e.g. the auto-update sweep telling
    /// the user an update needs approval), so it bypasses the per-worker
    /// capability check. Errors (an empty or over-long title) are swallowed: a
    /// notification is best-effort and must never fail a caller.
    pub fn notify_host(
        &self,
        plugin_id: &str,
        tone: crate::plugin::ui_state::Tone,
        title: String,
        body: Option<String>,
    ) {
        let _ = self.ui.notify(plugin_id, tone, title, body, None);
    }
}

/// Per-worker call context: who is calling and what they were granted. Built
/// once when the worker connects, from its `LoadedPlugin`.
pub struct PluginRpcContext {
    pub plugin_id: String,
    pub granted_capabilities: Vec<String>,
    /// The `(slot, id)` pairs the plugin declared in its manifest `ui` section.
    /// A `ui.state.set`/`ui.state.remove` for a pair not in this set is refused:
    /// a plugin can only fill the slots it declared.
    pub ui_contributions: HashSet<(UiSlot, String)>,
    /// This worker spawn's UI generation, stamped on every `ui.state.*` write so
    /// a stale worker cannot resurrect state after it exited.
    pub ui_generation: u64,
}

impl PluginRpcContext {
    /// Refuse the call unless the plugin holds `capability`.
    fn require(&self, capability: &str) -> Result<(), DispatchError> {
        if self.granted_capabilities.iter().any(|c| c == capability) {
            Ok(())
        } else {
            Err(DispatchError {
                code: codes::FORBIDDEN,
                message: format!(
                    "plugin {} did not declare or was not granted capability {capability:?}",
                    self.plugin_id
                ),
            })
        }
    }
}

/// A failed dispatch, carrying the JSON-RPC error code and message to return.
#[derive(Debug)]
pub struct DispatchError {
    pub code: i64,
    pub message: String,
}

impl DispatchError {
    fn invalid_params(msg: impl Into<String>) -> Self {
        Self {
            code: codes::INVALID_PARAMS,
            message: msg.into(),
        }
    }
    fn internal(msg: impl Into<String>) -> Self {
        Self {
            code: codes::INTERNAL_ERROR,
            message: msg.into(),
        }
    }
    fn method_not_found(method: &str) -> Self {
        Self {
            code: codes::METHOD_NOT_FOUND,
            message: format!("unknown method {method:?}"),
        }
    }
}

/// Dispatch one request to its handler after the capability check. Returns the
/// JSON result on success, or a [`DispatchError`] the transport turns into a
/// JSON-RPC error response.
pub fn dispatch(
    state: &HostApiState,
    ctx: &PluginRpcContext,
    method: &str,
    params: &Value,
) -> Result<Value, DispatchError> {
    match method {
        "events.publish" => {
            ctx.require(CAP_WORKER)?;
            events_publish(state, params)
        }
        "events.subscribe" => {
            ctx.require(CAP_WORKER)?;
            events_subscribe(state, params)
        }
        "session.meta.get" => {
            ctx.require(CAP_SESSION_READ)?;
            session_meta_get(state, ctx, params)
        }
        "session.meta.set" => {
            ctx.require(CAP_SESSION_WRITE)?;
            session_meta_set(state, ctx, params)
        }
        "session.meta.cas" => {
            ctx.require(CAP_SESSION_WRITE)?;
            session_meta_cas(state, ctx, params)
        }
        "sessions.list" => {
            ctx.require(CAP_SESSION_READ)?;
            sessions_list(state, params)
        }
        "config.get" => {
            ctx.require(CAP_WORKER)?;
            config_get(ctx, params)
        }
        "ui.state.set" => {
            ctx.require(CAP_WORKER)?;
            ui_state_set(state, ctx, params)
        }
        "ui.state.remove" => {
            ctx.require(CAP_WORKER)?;
            ui_state_remove(state, ctx, params)
        }
        "ui.notify" => {
            ctx.require(CAP_NOTIFICATIONS)?;
            ui_notify(state, ctx, params)
        }
        other => Err(DispatchError::method_not_found(other)),
    }
}

fn str_param<'a>(params: &'a Value, key: &str) -> Result<&'a str, DispatchError> {
    params
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| DispatchError::invalid_params(format!("missing string param {key:?}")))
}

/// An optional string param: absent or `null` is `None`, a string is `Some`, and
/// any other JSON type is a hard error. Reading these (`session_id`, `body`)
/// with a bare `as_str` would silently treat a non-string as absent, which can
/// turn a malformed per-session call into a global one; rejecting keeps the wire
/// contract honest.
fn optional_str_param<'a>(params: &'a Value, key: &str) -> Result<Option<&'a str>, DispatchError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        Some(_) => Err(DispatchError::invalid_params(format!(
            "param {key:?} must be a string"
        ))),
    }
}

fn events_publish(state: &HostApiState, params: &Value) -> Result<Value, DispatchError> {
    let topic = str_param(params, "topic")?;
    let payload = params
        .get("payload")
        .ok_or_else(|| DispatchError::invalid_params("missing param \"payload\""))?;
    let payload_json =
        serde_json::to_string(payload).map_err(|e| DispatchError::internal(e.to_string()))?;
    let conn = state.events.lock().unwrap_or_else(|p| p.into_inner());
    // The host assigns the seq, so a worker cannot forge ordering. Serialized
    // by the connection mutex, so highest_seq + 1 is race-free within the host.
    let seq = events::highest_seq(&conn, &state.schema, topic) + 1;
    let created_at = chrono::Utc::now().timestamp_millis();
    events::insert_event(&conn, &state.schema, topic, seq, &payload_json, created_at)
        .map_err(|e| DispatchError::internal(e.to_string()))?;
    events::prune_retention(&conn, &state.schema, topic, state.retention, &[]);
    Ok(json!({ "seq": seq }))
}

fn events_subscribe(state: &HostApiState, params: &Value) -> Result<Value, DispatchError> {
    let topics = params
        .get("topics")
        .and_then(Value::as_array)
        .ok_or_else(|| DispatchError::invalid_params("missing array param \"topics\""))?;
    // `after_seq` is a single cursor, but each topic carries its own seq
    // sequence (events_publish allocates per topic). Returning one `high_seq`
    // across several topics would let a client advance past a slower topic and
    // skip its later events. Until the response carries per-topic cursors, v1
    // accepts exactly one topic per call.
    if topics.len() != 1 {
        return Err(DispatchError::invalid_params(
            "\"topics\" currently supports exactly one topic; per-topic cursors are not implemented yet",
        ));
    }
    let after_seq = params.get("after_seq").and_then(Value::as_u64).unwrap_or(0);

    let conn = state.events.lock().unwrap_or_else(|p| p.into_inner());
    let mut out = Vec::new();
    let mut high_seq = after_seq;
    for topic in topics {
        let Some(topic) = topic.as_str() else {
            return Err(DispatchError::invalid_params("\"topics\" must be strings"));
        };
        for (seq, payload_json) in events::scan(
            &conn,
            &state.schema,
            topic,
            SeqBound::After(after_seq),
            Order::Asc,
            None,
        ) {
            high_seq = high_seq.max(seq);
            let payload: Value = serde_json::from_str(&payload_json).unwrap_or(Value::Null);
            out.push(json!({ "topic": topic, "seq": seq, "payload": payload }));
        }
    }
    Ok(json!({ "events": out, "high_seq": high_seq }))
}

/// Read this plugin's metadata object for `session_id` (its own namespaced
/// slot), or `Value::Null` when the session or slot is absent.
fn session_meta_get(
    state: &HostApiState,
    ctx: &PluginRpcContext,
    params: &Value,
) -> Result<Value, DispatchError> {
    let session_id = str_param(params, "session_id")?;
    let key = str_param(params, "key")?;
    let storage = state
        .storage()
        .map_err(|e| DispatchError::internal(e.to_string()))?;
    let instances = storage
        .load()
        .map_err(|e| DispatchError::internal(e.to_string()))?;
    let inst = instances
        .iter()
        .find(|i| i.id == session_id)
        .ok_or_else(|| DispatchError::invalid_params(format!("unknown session {session_id:?}")))?;
    let value = inst
        .plugin_meta
        .get(&ctx.plugin_id)
        .and_then(|slot| slot.get(key))
        .cloned()
        .unwrap_or(Value::Null);
    Ok(json!({ "value": value }))
}

fn session_meta_set(
    state: &HostApiState,
    ctx: &PluginRpcContext,
    params: &Value,
) -> Result<Value, DispatchError> {
    let session_id = str_param(params, "session_id")?.to_string();
    let key = str_param(params, "key")?.to_string();
    let value = params
        .get("value")
        .cloned()
        .ok_or_else(|| DispatchError::invalid_params("missing param \"value\""))?;
    let plugin_id = ctx.plugin_id.clone();
    let storage = state
        .storage()
        .map_err(|e| DispatchError::internal(e.to_string()))?;
    // An unknown session is bad caller input, not a host failure, so the
    // closure reports it as Ok(false) and we map that to INVALID_PARAMS,
    // matching session_meta_get. Only a genuine storage error is INTERNAL.
    let found = storage
        .update(|instances, _groups| {
            let Some(inst) = instances.iter_mut().find(|i| i.id == session_id) else {
                return Ok(false);
            };
            set_in_slot(inst, &plugin_id, &key, value.clone());
            Ok(true)
        })
        .map_err(|e| DispatchError::internal(e.to_string()))?;
    if !found {
        return Err(DispatchError::invalid_params(format!(
            "unknown session {session_id:?}"
        )));
    }
    Ok(json!({ "ok": true }))
}

/// Compare-and-swap a key in this plugin's slot: write `value` only if the
/// current value equals `expected`. Returns `{ swapped, current }` so a losing
/// writer sees the value that beat it rather than clobbering it.
fn session_meta_cas(
    state: &HostApiState,
    ctx: &PluginRpcContext,
    params: &Value,
) -> Result<Value, DispatchError> {
    let session_id = str_param(params, "session_id")?.to_string();
    let key = str_param(params, "key")?.to_string();
    let expected = params.get("expected").cloned().unwrap_or(Value::Null);
    let value = params
        .get("value")
        .cloned()
        .ok_or_else(|| DispatchError::invalid_params("missing param \"value\""))?;
    let plugin_id = ctx.plugin_id.clone();
    let storage = state
        .storage()
        .map_err(|e| DispatchError::internal(e.to_string()))?;
    // Ok(None) means the session does not exist (bad caller input ->
    // INVALID_PARAMS, like session_meta_get); Ok(Some(..)) carries the result.
    let outcome = storage
        .update(|instances, _groups| {
            let Some(inst) = instances.iter_mut().find(|i| i.id == session_id) else {
                return Ok(None);
            };
            let current = inst
                .plugin_meta
                .get(&plugin_id)
                .and_then(|slot| slot.get(&key))
                .cloned()
                .unwrap_or(Value::Null);
            if current == expected {
                set_in_slot(inst, &plugin_id, &key, value.clone());
                Ok(Some((true, value.clone())))
            } else {
                Ok(Some((false, current)))
            }
        })
        .map_err(|e| DispatchError::internal(e.to_string()))?;
    let (swapped, current) = outcome
        .ok_or_else(|| DispatchError::invalid_params(format!("unknown session {session_id:?}")))?;
    Ok(json!({ "swapped": swapped, "current": current }))
}

/// The set of inactivity states a `sessions.list` caller wants dropped from the
/// result. Each maps to an `is_*` predicate on the instance. Absent/empty means
/// return everything, so an old caller that passes no `exclude` is unaffected.
#[derive(Default)]
struct SessionListExclude {
    archived: bool,
    snoozed: bool,
    trashed: bool,
}

/// Parse the optional `exclude` param: an array of state names to drop, from
/// `archived` / `snoozed` / `trashed`. A missing or null `exclude` excludes
/// nothing. A non-array, a non-string entry, or an unknown name is caller error
/// (INVALID_PARAMS) rather than a silently-ignored typo, so a worker learns its
/// filter did not apply instead of assuming it did.
fn parse_sessions_exclude(params: &Value) -> Result<SessionListExclude, DispatchError> {
    let mut out = SessionListExclude::default();
    let raw = match params.get("exclude") {
        None | Some(Value::Null) => return Ok(out),
        Some(v) => v,
    };
    let arr = raw
        .as_array()
        .ok_or_else(|| DispatchError::invalid_params("param \"exclude\" must be an array"))?;
    for item in arr {
        match item.as_str() {
            Some("archived") => out.archived = true,
            Some("snoozed") => out.snoozed = true,
            Some("trashed") => out.trashed = true,
            Some(other) => {
                return Err(DispatchError::invalid_params(format!(
                    "unknown sessions.list exclude value {other:?}"
                )))
            }
            None => {
                return Err(DispatchError::invalid_params(
                    "\"exclude\" entries must be strings",
                ))
            }
        }
    }
    Ok(out)
}

fn sessions_list(state: &HostApiState, params: &Value) -> Result<Value, DispatchError> {
    let exclude = parse_sessions_exclude(params)?;
    let storage = state
        .storage()
        .map_err(|e| DispatchError::internal(e.to_string()))?;
    let instances = storage
        .load()
        .map_err(|e| DispatchError::internal(e.to_string()))?;
    let sessions: Vec<Value> = instances
        .iter()
        .filter(|i| {
            !((exclude.archived && i.is_archived())
                || (exclude.snoozed && i.is_snoozed())
                || (exclude.trashed && i.is_trashed()))
        })
        .map(|i| {
            json!({
                "id": i.id,
                "title": i.title,
                "project_path": i.project_path,
                "tool": i.tool,
                "status": format!("{:?}", i.status),
                "archived": i.is_archived(),
                "snoozed": i.is_snoozed(),
            })
        })
        .collect();
    Ok(json!({ "sessions": sessions }))
}

/// Read `plugins.<plugin_id>.settings.<key>` for the calling plugin's own id,
/// or `Value::Null` when the plugin has no config entry or the key is unset, so
/// the worker can fall back to its own default. The id is always the caller's
/// own ([`PluginRpcContext::plugin_id`]), never a request parameter, so one
/// plugin can never read another's settings.
fn config_get(ctx: &PluginRpcContext, params: &Value) -> Result<Value, DispatchError> {
    let key = str_param(params, "key")?;
    let config =
        crate::session::Config::load().map_err(|e| DispatchError::internal(e.to_string()))?;
    let value = match config
        .plugins
        .get(&ctx.plugin_id)
        .and_then(|plugin| plugin.settings.get(key))
    {
        // The stored value is TOML; hand it back to the worker as JSON.
        Some(toml_value) => {
            serde_json::to_value(toml_value).map_err(|e| DispatchError::internal(e.to_string()))?
        }
        None => Value::Null,
    };
    Ok(json!({ "value": value }))
}

/// Parse the `slot` param into a typed [`UiSlot`]. An unknown slot is bad
/// input, not a host failure.
fn parse_ui_slot(params: &Value) -> Result<UiSlot, DispatchError> {
    let raw = params
        .get("slot")
        .ok_or_else(|| DispatchError::invalid_params("missing string param \"slot\""))?;
    serde_json::from_value::<UiSlot>(raw.clone())
        .map_err(|_| DispatchError::invalid_params(format!("unknown ui slot {raw}")))
}

/// Map a store-level [`UiError`] to a JSON-RPC error. A bad payload/scope is the
/// caller's input (INVALID_PARAMS); a quota or stale-generation refusal is the
/// host declining the mutation (FORBIDDEN, our reserved code).
fn ui_dispatch_error(e: UiError) -> DispatchError {
    match e {
        UiError::BadRequest(message) => DispatchError::invalid_params(message),
        UiError::QuotaExceeded => DispatchError {
            code: codes::FORBIDDEN,
            message: "plugin UI-state quota exceeded".into(),
        },
        UiError::StaleWorker => DispatchError {
            code: codes::FORBIDDEN,
            message: "worker generation is no longer active".into(),
        },
    }
}

/// Refuse a `ui.state.*` call unless the plugin declared this `(slot, id)` in
/// its manifest `ui` section. This, plus `runtime.worker`, is the full gate on
/// pushing UI state; no dedicated `ui` capability is introduced.
fn require_declared_slot(
    ctx: &PluginRpcContext,
    slot: UiSlot,
    id: &str,
) -> Result<(), DispatchError> {
    if ctx.ui_contributions.contains(&(slot, id.to_string())) {
        Ok(())
    } else {
        Err(DispatchError {
            code: codes::FORBIDDEN,
            message: format!(
                "plugin {} did not declare ui slot {slot:?} with id {id:?}",
                ctx.plugin_id
            ),
        })
    }
}

fn ui_state_set(
    state: &HostApiState,
    ctx: &PluginRpcContext,
    params: &Value,
) -> Result<Value, DispatchError> {
    let slot = parse_ui_slot(params)?;
    let id = str_param(params, "id")?;
    require_declared_slot(ctx, slot, id)?;
    let session_id = optional_str_param(params, "session_id")?;
    let payload = params
        .get("payload")
        .ok_or_else(|| DispatchError::invalid_params("missing param \"payload\""))?;
    state
        .ui
        .set(
            &ctx.plugin_id,
            ctx.ui_generation,
            slot,
            id,
            session_id,
            payload,
        )
        .map_err(ui_dispatch_error)?;
    Ok(json!({ "ok": true }))
}

fn ui_state_remove(
    state: &HostApiState,
    ctx: &PluginRpcContext,
    params: &Value,
) -> Result<Value, DispatchError> {
    let slot = parse_ui_slot(params)?;
    let id = str_param(params, "id")?;
    require_declared_slot(ctx, slot, id)?;
    let session_id = optional_str_param(params, "session_id")?;
    state
        .ui
        .remove(&ctx.plugin_id, ctx.ui_generation, slot, id, session_id)
        .map_err(ui_dispatch_error)?;
    Ok(json!({ "ok": true }))
}

fn ui_notify(
    state: &HostApiState,
    ctx: &PluginRpcContext,
    params: &Value,
) -> Result<Value, DispatchError> {
    let title = str_param(params, "title")?.to_string();
    let body = optional_str_param(params, "body")?.map(str::to_string);
    let session_id = optional_str_param(params, "session_id")?.map(str::to_string);
    let tone = match params.get("tone") {
        None => Tone::Info,
        Some(v) => serde_json::from_value::<Tone>(v.clone())
            .map_err(|_| DispatchError::invalid_params(format!("unknown tone {v}")))?,
    };
    let seq = state
        .ui
        .notify(&ctx.plugin_id, tone, title, body, session_id)
        .map_err(ui_dispatch_error)?;
    Ok(json!({ "seq": seq }))
}

/// Set `key = value` inside `inst.plugin_meta[plugin_id]`, creating the slot as
/// a JSON object if absent. The slot is namespaced to the plugin id, never a
/// request parameter, which is what keeps one plugin out of another's data.
fn set_in_slot(inst: &mut crate::session::Instance, plugin_id: &str, key: &str, value: Value) {
    let slot = inst
        .plugin_meta
        .entry(plugin_id.to_string())
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    if !slot.is_object() {
        *slot = Value::Object(serde_json::Map::new());
    }
    if let Some(map) = slot.as_object_mut() {
        map.insert(key.to_string(), value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(caps: &[&str]) -> PluginRpcContext {
        PluginRpcContext {
            plugin_id: "acme.worker".to_string(),
            granted_capabilities: caps.iter().map(|c| c.to_string()).collect(),
            ui_contributions: HashSet::new(),
            ui_generation: 0,
        }
    }

    fn state(dir: &std::path::Path) -> HostApiState {
        HostApiState::open(&dir.join("plugin_events.db"), "default", 100).unwrap()
    }

    #[test]
    fn ungranted_capability_is_forbidden() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state(tmp.path());
        // No capabilities granted: even events.publish is refused.
        let err = dispatch(
            &state,
            &ctx(&[]),
            "events.publish",
            &json!({"topic": "t", "payload": {}}),
        )
        .unwrap_err();
        assert_eq!(err.code, codes::FORBIDDEN);

        // session.meta.set requires session.write specifically.
        let err = dispatch(
            &state,
            &ctx(&[CAP_SESSION_READ]),
            "session.meta.set",
            &json!({"session_id": "s", "key": "k", "value": 1}),
        )
        .unwrap_err();
        assert_eq!(err.code, codes::FORBIDDEN);
    }

    #[test]
    fn unknown_method_is_method_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state(tmp.path());
        let err = dispatch(&state, &ctx(&[CAP_WORKER]), "no.such", &json!({})).unwrap_err();
        assert_eq!(err.code, codes::METHOD_NOT_FOUND);
    }

    #[test]
    fn events_publish_then_subscribe_replays_after_cursor() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state(tmp.path());
        let c = ctx(&[CAP_WORKER]);
        for n in 1..=3 {
            dispatch(
                &state,
                &c,
                "events.publish",
                &json!({"topic": "build", "payload": {"n": n}}),
            )
            .unwrap();
        }
        // Subscribe after seq 1: see seq 2 and 3 only.
        let got = dispatch(
            &state,
            &c,
            "events.subscribe",
            &json!({"topics": ["build"], "after_seq": 1}),
        )
        .unwrap();
        let events = got["events"].as_array().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["seq"], json!(2));
        assert_eq!(events[0]["payload"]["n"], json!(2));
        assert_eq!(got["high_seq"], json!(3));
    }

    /// Session metadata round-trip against real session storage: set, get, a
    /// compare-and-swap that loses and one that wins, per-plugin namespace
    /// isolation, and sessions.list. Isolated under a temp `XDG_CONFIG_HOME` so
    /// it never touches real user state; serial because it mutates the env.
    #[test]
    #[serial_test::serial]
    fn session_meta_cas_namespace_and_list() {
        use crate::session::{Instance, Storage};

        // Restore XDG_CONFIG_HOME on drop, so a failing assertion does not leak
        // the override into the rest of the test process.
        struct XdgGuard(Option<std::ffi::OsString>);
        impl Drop for XdgGuard {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                    None => std::env::remove_var("XDG_CONFIG_HOME"),
                }
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let _xdg = XdgGuard(std::env::var_os("XDG_CONFIG_HOME"));
        std::env::set_var("XDG_CONFIG_HOME", tmp.path());

        // Seed one session in the default profile's storage.
        let storage = Storage::new_unwatched("default").unwrap();
        let session_id = storage
            .update(|instances, _groups| {
                let inst = Instance::new("sess", "/tmp/plugin-host-test");
                let id = inst.id.clone();
                instances.push(inst);
                Ok(id)
            })
            .unwrap();

        let state =
            HostApiState::open(&tmp.path().join("plugin_events.db"), "default", 100).unwrap();
        let a = ctx(&[CAP_SESSION_READ, CAP_SESSION_WRITE]);

        // set then get.
        dispatch(
            &state,
            &a,
            "session.meta.set",
            &json!({"session_id": session_id, "key": "k", "value": 42}),
        )
        .unwrap();
        let got = dispatch(
            &state,
            &a,
            "session.meta.get",
            &json!({"session_id": session_id, "key": "k"}),
        )
        .unwrap();
        assert_eq!(got["value"], json!(42));

        // CAS that loses (wrong expected) reports the current value, no clobber.
        let lose = dispatch(
            &state,
            &a,
            "session.meta.cas",
            &json!({"session_id": session_id, "key": "k", "expected": 0, "value": 99}),
        )
        .unwrap();
        assert_eq!(lose["swapped"], json!(false));
        assert_eq!(lose["current"], json!(42));

        // CAS that wins.
        let win = dispatch(
            &state,
            &a,
            "session.meta.cas",
            &json!({"session_id": session_id, "key": "k", "expected": 42, "value": 99}),
        )
        .unwrap();
        assert_eq!(win["swapped"], json!(true));

        // A different plugin cannot see plugin "acme.worker"'s slot.
        let b = PluginRpcContext {
            plugin_id: "other.plugin".to_string(),
            granted_capabilities: vec![CAP_SESSION_READ.to_string()],
            ui_contributions: HashSet::new(),
            ui_generation: 0,
        };
        let other = dispatch(
            &state,
            &b,
            "session.meta.get",
            &json!({"session_id": session_id, "key": "k"}),
        )
        .unwrap();
        assert_eq!(other["value"], json!(null));

        // sessions.list surfaces the seeded session; an active session reports
        // neither archived nor snoozed.
        let list = dispatch(&state, &a, "sessions.list", &json!({})).unwrap();
        let sessions = list["sessions"].as_array().unwrap();
        let seeded = sessions
            .iter()
            .find(|s| s["id"] == json!(session_id))
            .unwrap();
        assert_eq!(seeded["archived"], json!(false));
        assert_eq!(seeded["snoozed"], json!(false));
    }

    /// `sessions.list` exposes the archive/snooze state per entry so a worker can
    /// skip inactive sessions. A past snooze deadline reports `snoozed: false`
    /// (snooze is deadline-based; only a future deadline counts as active).
    #[test]
    #[serial_test::serial]
    fn sessions_list_exposes_archived_and_snoozed_flags() {
        use crate::session::{Instance, Storage};

        struct XdgGuard(Option<std::ffi::OsString>);
        impl Drop for XdgGuard {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                    None => std::env::remove_var("XDG_CONFIG_HOME"),
                }
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let _xdg = XdgGuard(std::env::var_os("XDG_CONFIG_HOME"));
        std::env::set_var("XDG_CONFIG_HOME", tmp.path());

        let storage = Storage::new_unwatched("default").unwrap();
        let (archived_id, future_id, past_id) = storage
            .update(|instances, _groups| {
                let mut archived = Instance::new("archived", "/tmp/plugin-host-test");
                archived.archived_at = Some(chrono::Utc::now());
                let archived_id = archived.id.clone();

                let mut future = Instance::new("future-snooze", "/tmp/plugin-host-test");
                future.snoozed_until = Some(chrono::Utc::now() + chrono::Duration::hours(1));
                let future_id = future.id.clone();

                let mut past = Instance::new("past-snooze", "/tmp/plugin-host-test");
                past.snoozed_until = Some(chrono::Utc::now() - chrono::Duration::hours(1));
                let past_id = past.id.clone();

                instances.push(archived);
                instances.push(future);
                instances.push(past);
                Ok((archived_id, future_id, past_id))
            })
            .unwrap();

        let state =
            HostApiState::open(&tmp.path().join("plugin_events.db"), "default", 100).unwrap();
        let a = ctx(&[CAP_SESSION_READ]);

        let list = dispatch(&state, &a, "sessions.list", &json!({})).unwrap();
        let sessions = list["sessions"].as_array().unwrap();
        let by_id = |id: &str| sessions.iter().find(|s| s["id"] == json!(id)).unwrap();

        assert_eq!(by_id(&archived_id)["archived"], json!(true));
        assert_eq!(by_id(&archived_id)["snoozed"], json!(false));

        assert_eq!(by_id(&future_id)["snoozed"], json!(true));
        assert_eq!(by_id(&future_id)["archived"], json!(false));

        // A snooze deadline in the past is inactive.
        assert_eq!(by_id(&past_id)["snoozed"], json!(false));
        assert_eq!(by_id(&past_id)["archived"], json!(false));
    }

    /// `sessions.list` drops the states named in `exclude` server-side, so a
    /// worker that only cares about live sessions never enumerates dormant or
    /// trashed ones. Each state filters independently and the flags on the
    /// returned entries are unaffected.
    #[test]
    #[serial_test::serial]
    fn sessions_list_exclude_filters_server_side() {
        use crate::session::{Instance, Storage};

        struct XdgGuard(Option<std::ffi::OsString>);
        impl Drop for XdgGuard {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                    None => std::env::remove_var("XDG_CONFIG_HOME"),
                }
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let _xdg = XdgGuard(std::env::var_os("XDG_CONFIG_HOME"));
        std::env::set_var("XDG_CONFIG_HOME", tmp.path());

        let storage = Storage::new_unwatched("default").unwrap();
        let (active_id, archived_id, snoozed_id, trashed_id) = storage
            .update(|instances, _groups| {
                let active = Instance::new("active", "/tmp/plugin-host-test");
                let active_id = active.id.clone();

                let mut archived = Instance::new("archived", "/tmp/plugin-host-test");
                archived.archived_at = Some(chrono::Utc::now());
                let archived_id = archived.id.clone();

                let mut snoozed = Instance::new("snoozed", "/tmp/plugin-host-test");
                snoozed.snoozed_until = Some(chrono::Utc::now() + chrono::Duration::hours(1));
                let snoozed_id = snoozed.id.clone();

                let mut trashed = Instance::new("trashed", "/tmp/plugin-host-test");
                trashed.trashed_at = Some(chrono::Utc::now());
                let trashed_id = trashed.id.clone();

                instances.push(active);
                instances.push(archived);
                instances.push(snoozed);
                instances.push(trashed);
                Ok((active_id, archived_id, snoozed_id, trashed_id))
            })
            .unwrap();

        let state =
            HostApiState::open(&tmp.path().join("plugin_events.db"), "default", 100).unwrap();
        let a = ctx(&[CAP_SESSION_READ]);
        // Assert on the ids this test seeded rather than on totals: the store is
        // the profile's real storage (an existing app dir wins over the test's
        // XDG override on macOS), so ambient sessions may be present.
        let ids = |v: &Value| -> Vec<String> {
            v["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .map(|s| s["id"].as_str().unwrap().to_string())
                .collect()
        };

        // No exclude: all four seeded sessions come back.
        let all = ids(&dispatch(&state, &a, "sessions.list", &json!({})).unwrap());
        for id in [&active_id, &archived_id, &snoozed_id, &trashed_id] {
            assert!(all.contains(id), "no-exclude list missing {id}");
        }

        // Each exclude drops exactly its state and leaves the others.
        let no_trash = dispatch(
            &state,
            &a,
            "sessions.list",
            &json!({ "exclude": ["trashed"] }),
        )
        .unwrap();
        let no_trash_ids = ids(&no_trash);
        assert!(!no_trash_ids.contains(&trashed_id));
        assert!(no_trash_ids.contains(&archived_id));
        assert!(no_trash_ids.contains(&active_id));
        // Flags on returned entries are unaffected by filtering.
        let archived_entry = no_trash["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == json!(archived_id))
            .unwrap();
        assert_eq!(archived_entry["archived"], json!(true));

        let no_archived = ids(&dispatch(
            &state,
            &a,
            "sessions.list",
            &json!({ "exclude": ["archived"] }),
        )
        .unwrap());
        assert!(!no_archived.contains(&archived_id));
        assert!(no_archived.contains(&trashed_id));

        // Excluding every dormant state drops all three, keeps the active one.
        let live = ids(&dispatch(
            &state,
            &a,
            "sessions.list",
            &json!({ "exclude": ["archived", "snoozed", "trashed"] }),
        )
        .unwrap());
        assert!(live.contains(&active_id));
        for id in [&archived_id, &snoozed_id, &trashed_id] {
            assert!(!live.contains(id), "dormant {id} should be excluded");
        }

        // Drop exactly the ids this test seeded so it leaves no residue in the
        // profile store, matching the deterministic-and-self-cleaning rule.
        let seeded = [&active_id, &archived_id, &snoozed_id, &trashed_id];
        storage
            .update(|instances, _groups| {
                instances.retain(|i| !seeded.iter().any(|id| **id == i.id));
                Ok(())
            })
            .unwrap();
    }

    /// A malformed `exclude` is caller error (INVALID_PARAMS), not a silently
    /// ignored no-op, so a worker's typo cannot masquerade as an applied filter.
    #[test]
    #[serial_test::serial]
    fn sessions_list_rejects_bad_exclude() {
        let tmp = tempfile::tempdir().unwrap();
        let state =
            HostApiState::open(&tmp.path().join("plugin_events.db"), "default", 100).unwrap();
        let a = ctx(&[CAP_SESSION_READ]);

        for bad in [
            json!({ "exclude": "trashed" }),
            json!({ "exclude": ["deleted"] }),
            json!({ "exclude": [1] }),
        ] {
            let err = dispatch(&state, &a, "sessions.list", &bad).unwrap_err();
            assert_eq!(err.code, codes::INVALID_PARAMS);
        }
    }

    /// `config.get` reads the calling plugin's own persisted settings, gated by
    /// `runtime.worker`: a granted worker reads its value, an unset key returns
    /// null, a different plugin id cannot see it, and a worker without
    /// `runtime.worker` is refused. Isolated under a temp `XDG_CONFIG_HOME` so it
    /// never touches real user config; serial because it mutates the env.
    #[test]
    #[serial_test::serial]
    fn config_get_scopes_to_caller_and_requires_worker() {
        use crate::session::{save_config, Config, PluginConfig};

        let tmp = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("XDG_CONFIG_HOME");
        std::env::set_var("XDG_CONFIG_HOME", tmp.path());

        // Seed the global config with one setting under "acme.worker".
        let mut config = Config::default();
        let mut plugin = PluginConfig::default();
        plugin
            .settings
            .insert("poll_interval_ms".to_string(), toml::Value::Integer(5000));
        config.plugins.insert("acme.worker".to_string(), plugin);
        save_config(&config).unwrap();

        let state = state(tmp.path());
        let worker = ctx(&[CAP_WORKER]);

        // The owning plugin reads its own setting back as JSON.
        let got = dispatch(
            &state,
            &worker,
            "config.get",
            &json!({"key": "poll_interval_ms"}),
        )
        .unwrap();
        assert_eq!(got["value"], json!(5000));

        // An unset key returns null so the worker falls back to its default.
        let missing = dispatch(&state, &worker, "config.get", &json!({"key": "nope"})).unwrap();
        assert_eq!(missing["value"], json!(null));

        // A different plugin id cannot see "acme.worker"'s settings.
        let other = PluginRpcContext {
            plugin_id: "other.plugin".to_string(),
            granted_capabilities: vec![CAP_WORKER.to_string()],
            ui_contributions: HashSet::new(),
            ui_generation: 0,
        };
        let other_got = dispatch(
            &state,
            &other,
            "config.get",
            &json!({"key": "poll_interval_ms"}),
        )
        .unwrap();
        assert_eq!(other_got["value"], json!(null));

        // Without runtime.worker the call is forbidden.
        let err = dispatch(
            &state,
            &ctx(&[CAP_SESSION_READ]),
            "config.get",
            &json!({"key": "poll_interval_ms"}),
        )
        .unwrap_err();
        assert_eq!(err.code, codes::FORBIDDEN);

        match prev {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
    }

    /// Build a context that declared a single `(slot, id)` UI contribution and
    /// holds the given capabilities, registered against `state` so its
    /// generation is the active one.
    fn ui_ctx(state: &HostApiState, caps: &[&str], slot: UiSlot, id: &str) -> PluginRpcContext {
        let mut contributions = HashSet::new();
        contributions.insert((slot, id.to_string()));
        PluginRpcContext {
            plugin_id: "acme.worker".to_string(),
            granted_capabilities: caps.iter().map(|c| c.to_string()).collect(),
            ui_contributions: contributions,
            ui_generation: state.begin_ui_generation("acme.worker"),
        }
    }

    #[test]
    fn ui_state_set_requires_declared_slot() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state(tmp.path());
        // Declared status-bar/"main", but pushing row-badge/"main" is refused.
        let c = ui_ctx(&state, &[CAP_WORKER], UiSlot::StatusBar, "main");
        let err = dispatch(
            &state,
            &c,
            "ui.state.set",
            &json!({"slot": "row-badge", "id": "main", "session_id": "s1", "payload": {"text": "x"}}),
        )
        .unwrap_err();
        assert_eq!(err.code, codes::FORBIDDEN);

        // The declared slot succeeds and surfaces in the snapshot.
        dispatch(
            &state,
            &c,
            "ui.state.set",
            &json!({"slot": "status-bar", "id": "main", "payload": {"text": "ok", "tone": "success"}}),
        )
        .unwrap();
        let snap = state.ui_snapshot();
        assert_eq!(snap.entries.len(), 1);
        assert_eq!(snap.entries[0].payload["text"], json!("ok"));
    }

    #[test]
    fn ui_state_set_needs_worker_capability() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state(tmp.path());
        let c = ui_ctx(&state, &[], UiSlot::StatusBar, "main");
        let err = dispatch(
            &state,
            &c,
            "ui.state.set",
            &json!({"slot": "status-bar", "id": "main", "payload": {"text": "x"}}),
        )
        .unwrap_err();
        assert_eq!(err.code, codes::FORBIDDEN);
    }

    #[test]
    fn ui_notify_requires_notifications_capability() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state(tmp.path());
        // runtime.worker alone is not enough for ui.notify.
        let c = ui_ctx(&state, &[CAP_WORKER], UiSlot::Notification, "n");
        let err = dispatch(
            &state,
            &c,
            "ui.notify",
            &json!({"title": "Build failed", "tone": "danger"}),
        )
        .unwrap_err();
        assert_eq!(err.code, codes::FORBIDDEN);

        // With the capability it posts and returns a seq.
        let c = ui_ctx(&state, &[CAP_NOTIFICATIONS], UiSlot::Notification, "n");
        let ok = dispatch(
            &state,
            &c,
            "ui.notify",
            &json!({"title": "Build failed", "tone": "danger"}),
        )
        .unwrap();
        assert_eq!(ok["seq"], json!(1));
        assert_eq!(state.ui_snapshot().notifications.len(), 1);
    }

    #[test]
    fn ui_state_set_rejects_unknown_slot_and_bad_payload() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state(tmp.path());
        let c = ui_ctx(&state, &[CAP_WORKER], UiSlot::StatusBar, "main");
        // Unknown slot string.
        let err = dispatch(
            &state,
            &c,
            "ui.state.set",
            &json!({"slot": "sidebar", "id": "main", "payload": {"text": "x"}}),
        )
        .unwrap_err();
        assert_eq!(err.code, codes::INVALID_PARAMS);
        // Declared slot, malformed payload (missing required `text`).
        let err = dispatch(
            &state,
            &c,
            "ui.state.set",
            &json!({"slot": "status-bar", "id": "main", "payload": {"tone": "info"}}),
        )
        .unwrap_err();
        assert_eq!(err.code, codes::INVALID_PARAMS);
    }
}
