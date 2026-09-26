//! The capability-gated host API a plugin worker calls over the worker

use std::collections::HashSet;
use std::sync::Mutex;

use anyhow::Context as _;
use aoe_plugin_api::UiSlot;
use rusqlite::{Connection, OptionalExtension};
use serde_json::{json, Value};

use crate::events::{self, Order, Schema, SeqBound};
use crate::plugin::protocol::codes;
use crate::plugin::ui_state::{Tone, UiError, UiSnapshot, UiStore};
use crate::session::Storage;

const CAP_WORKER: &str = "runtime.worker";
const CAP_SESSION_READ: &str = "session.read";
const CAP_SESSION_WRITE: &str = "session.write";
const CAP_NOTIFICATIONS: &str = "notifications";
const CAP_COMPOSER_WRITE: &str = "composer.write";
const CAP_BROWSER_OPEN: &str = "browser_open";

const STORAGE_MAX_KEYS: usize = 64;
const STORAGE_MAX_KEY_BYTES: usize = 256;
const STORAGE_MAX_VALUE_BYTES: usize = 64 * 1024;

pub struct HostApiState {
    events: Mutex<Connection>,
    schema: Schema,
    retention: usize,
    profile: String,
    ui: UiStore,
    settings_revision: std::sync::atomic::AtomicU64,
}

impl HostApiState {
    pub fn open(
        db_path: &std::path::Path,
        profile: &str,
        retention: usize,
    ) -> anyhow::Result<Self> {
        let schema = Schema::new("plugin_host")?;
        let conn = events::open(db_path, &schema)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS plugin_storage (
                 plugin_id  TEXT NOT NULL,
                 key        TEXT NOT NULL,
                 value_json TEXT NOT NULL,
                 updated_at INTEGER NOT NULL,
                 PRIMARY KEY (plugin_id, key)
             );",
        )
        .context("create plugin_storage table")?;
        Ok(Self {
            events: Mutex::new(conn),
            schema,
            retention,
            profile: profile.to_string(),
            ui: UiStore::new(),
            settings_revision: std::sync::atomic::AtomicU64::new(0),
        })
    }

    fn storage(&self) -> anyhow::Result<Storage> {
        Storage::new_unwatched(&self.profile)
    }

    pub fn bump_settings_revision(&self) -> u64 {
        self.settings_revision
            .fetch_add(1, std::sync::atomic::Ordering::Release)
            + 1
    }

    fn settings_revision(&self) -> u64 {
        self.settings_revision
            .load(std::sync::atomic::Ordering::Acquire)
    }

    pub fn begin_ui_generation(&self, plugin_id: &str) -> u64 {
        self.ui.begin_generation(plugin_id)
    }

    pub fn clear_ui(&self, plugin_id: &str, generation: u64) -> bool {
        self.ui.clear_plugin(plugin_id, generation)
    }

    pub fn ui_snapshot(&self) -> UiSnapshot {
        self.ui.snapshot()
    }

    pub fn ui_revision(&self, plugin_id: &str, session_id: Option<&str>) -> u64 {
        self.ui.revision(plugin_id, session_id)
    }

    pub fn notify_host(
        &self,
        plugin_id: &str,
        tone: crate::plugin::ui_state::Tone,
        title: String,
        body: Option<String>,
    ) {
        let _ = self.ui.notify(plugin_id, tone, title, body, None, None);
    }
}

pub struct PluginRpcContext {
    pub plugin_id: String,
    pub granted_capabilities: Vec<String>,
    pub ui_contributions: HashSet<(UiSlot, String)>,
    pub ui_generation: u64,
}

impl PluginRpcContext {
    pub(crate) fn require(&self, capability: &str) -> Result<(), DispatchError> {
        if self.granted_capabilities.iter().any(|c| c == capability) {
            Ok(())
        } else {
            Err(DispatchError {
                code: codes::FORBIDDEN,
                message: format!(
                    "plugin {} did not declare or was not granted capability {capability:?}",
                    self.plugin_id
                ),
                data: Some(serde_json::json!({
                    "kind": "capability_missing",
                    "required_capability": capability,
                })),
            })
        }
    }
}

#[derive(Debug)]
pub struct DispatchError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

impl DispatchError {
    pub(crate) fn invalid_params(msg: impl Into<String>) -> Self {
        Self {
            code: codes::INVALID_PARAMS,
            message: msg.into(),
            data: None,
        }
    }
    pub(crate) fn internal(msg: impl Into<String>) -> Self {
        Self {
            code: codes::INTERNAL_ERROR,
            message: msg.into(),
            data: None,
        }
    }
    fn method_not_found(method: &str) -> Self {
        Self {
            code: codes::METHOD_NOT_FOUND,
            message: format!("unknown method {method:?}"),
            data: None,
        }
    }

    pub(crate) fn with_kind(code: i64, kind: &str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: Some(serde_json::json!({ "kind": kind })),
        }
    }
}

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
            config_get(state, ctx, params)
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
        "ui.open_url" => {
            ctx.require(CAP_BROWSER_OPEN)?;
            ui_open_url(state, ctx, params)
        }
        "plugin.storage.get" => {
            ctx.require(CAP_WORKER)?;
            plugin_storage_get(state, ctx, params)
        }
        "plugin.storage.set" => {
            ctx.require(CAP_WORKER)?;
            plugin_storage_set(state, ctx, params)
        }
        "plugin.storage.cas" => {
            ctx.require(CAP_WORKER)?;
            plugin_storage_cas(state, ctx, params)
        }
        "plugin.storage.remove" => {
            ctx.require(CAP_WORKER)?;
            plugin_storage_remove(state, ctx, params)
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

#[derive(Default)]
struct SessionListExclude {
    archived: bool,
    snoozed: bool,
    trashed: bool,
}

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

fn config_get(
    state: &HostApiState,
    ctx: &PluginRpcContext,
    params: &Value,
) -> Result<Value, DispatchError> {
    let key = str_param(params, "key")?;
    let mut value = Value::Null;
    let mut revision = state.settings_revision();
    // Retry when a settings write lands mid-load so the value and revision stay paired.
    for _ in 0..8 {
        let rev_before = revision;
        let config =
            crate::session::Config::load().map_err(|e| DispatchError::internal(e.to_string()))?;
        value = match config
            .plugins
            .get(&ctx.plugin_id)
            .and_then(|plugin| plugin.settings.get(key))
        {
            Some(toml_value) => serde_json::to_value(toml_value)
                .map_err(|e| DispatchError::internal(e.to_string()))?,
            None => Value::Null,
        };
        revision = state.settings_revision();
        if revision == rev_before {
            break;
        }
    }
    Ok(json!({ "value": value, "revision": revision }))
}

fn storage_key(params: &Value) -> Result<String, DispatchError> {
    let key = str_param(params, "key")?;
    if key.is_empty() {
        return Err(DispatchError::invalid_params(
            "storage key must be non-empty",
        ));
    }
    if key.len() > STORAGE_MAX_KEY_BYTES {
        return Err(DispatchError::with_kind(
            codes::FORBIDDEN,
            "storage_quota_exceeded",
            format!("storage key exceeds {STORAGE_MAX_KEY_BYTES} bytes"),
        ));
    }
    Ok(key.to_string())
}

fn storage_value(params: &Value) -> Result<String, DispatchError> {
    let value = params
        .get("value")
        .ok_or_else(|| DispatchError::invalid_params("missing param \"value\""))?;
    let json = serde_json::to_string(value)
        .map_err(|e| DispatchError::invalid_params(format!("value is not serializable: {e}")))?;
    if json.len() > STORAGE_MAX_VALUE_BYTES {
        return Err(DispatchError::with_kind(
            codes::FORBIDDEN,
            "storage_quota_exceeded",
            format!("storage value exceeds {STORAGE_MAX_VALUE_BYTES} bytes"),
        ));
    }
    Ok(json)
}

fn plugin_storage_get(
    state: &HostApiState,
    ctx: &PluginRpcContext,
    params: &Value,
) -> Result<Value, DispatchError> {
    let key = storage_key(params)?;
    let conn = state.events.lock().unwrap_or_else(|p| p.into_inner());
    let stored: Option<String> = conn
        .query_row(
            "SELECT value_json FROM plugin_storage WHERE plugin_id = ?1 AND key = ?2",
            rusqlite::params![ctx.plugin_id, key],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| DispatchError::internal(e.to_string()))?;
    let value = decode_stored(stored)?;
    Ok(json!({ "value": value }))
}

fn plugin_storage_set(
    state: &HostApiState,
    ctx: &PluginRpcContext,
    params: &Value,
) -> Result<Value, DispatchError> {
    let key = storage_key(params)?;
    let value_json = storage_value(params)?;
    let now = chrono::Utc::now().timestamp_millis();
    let conn = state.events.lock().unwrap_or_else(|p| p.into_inner());
    enforce_key_quota(&conn, &ctx.plugin_id, &key)?;
    conn.execute(
        "INSERT INTO plugin_storage (plugin_id, key, value_json, updated_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (plugin_id, key) DO UPDATE SET value_json = ?3, updated_at = ?4",
        rusqlite::params![ctx.plugin_id, key, value_json, now],
    )
    .map_err(|e| DispatchError::internal(e.to_string()))?;
    Ok(json!({}))
}

fn plugin_storage_cas(
    state: &HostApiState,
    ctx: &PluginRpcContext,
    params: &Value,
) -> Result<Value, DispatchError> {
    let key = storage_key(params)?;
    let value_json = storage_value(params)?;
    let expected = params
        .get("expected")
        .cloned()
        .ok_or_else(|| DispatchError::invalid_params("missing param \"expected\""))?;
    let now = chrono::Utc::now().timestamp_millis();
    let mut conn = state.events.lock().unwrap_or_else(|p| p.into_inner());
    let tx = conn
        .transaction()
        .map_err(|e| DispatchError::internal(e.to_string()))?;
    let stored: Option<String> = tx
        .query_row(
            "SELECT value_json FROM plugin_storage WHERE plugin_id = ?1 AND key = ?2",
            rusqlite::params![ctx.plugin_id, key],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| DispatchError::internal(e.to_string()))?;
    let current = decode_stored(stored)?;
    if current != expected {
        return Ok(json!({ "swapped": false, "current": current }));
    }
    enforce_key_quota_tx(&tx, &ctx.plugin_id, &key)?;
    tx.execute(
        "INSERT INTO plugin_storage (plugin_id, key, value_json, updated_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (plugin_id, key) DO UPDATE SET value_json = ?3, updated_at = ?4",
        rusqlite::params![ctx.plugin_id, key, value_json, now],
    )
    .map_err(|e| DispatchError::internal(e.to_string()))?;
    let new_value: Value =
        serde_json::from_str(&value_json).map_err(|e| DispatchError::internal(e.to_string()))?;
    tx.commit()
        .map_err(|e| DispatchError::internal(e.to_string()))?;
    Ok(json!({ "swapped": true, "current": new_value }))
}

fn plugin_storage_remove(
    state: &HostApiState,
    ctx: &PluginRpcContext,
    params: &Value,
) -> Result<Value, DispatchError> {
    let key = storage_key(params)?;
    let conn = state.events.lock().unwrap_or_else(|p| p.into_inner());
    let removed = conn
        .execute(
            "DELETE FROM plugin_storage WHERE plugin_id = ?1 AND key = ?2",
            rusqlite::params![ctx.plugin_id, key],
        )
        .map_err(|e| DispatchError::internal(e.to_string()))?;
    Ok(json!({ "removed": removed > 0 }))
}

fn decode_stored(stored: Option<String>) -> Result<Value, DispatchError> {
    match stored {
        Some(json) => {
            serde_json::from_str(&json).map_err(|e| DispatchError::internal(e.to_string()))
        }
        None => Ok(Value::Null),
    }
}

fn enforce_key_quota(conn: &Connection, plugin_id: &str, key: &str) -> Result<(), DispatchError> {
    let exists: bool = conn
        .query_row(
            "SELECT 1 FROM plugin_storage WHERE plugin_id = ?1 AND key = ?2",
            rusqlite::params![plugin_id, key],
            |_| Ok(()),
        )
        .optional()
        .map_err(|e| DispatchError::internal(e.to_string()))?
        .is_some();
    if exists {
        return Ok(());
    }
    let count: usize = conn
        .query_row(
            "SELECT COUNT(*) FROM plugin_storage WHERE plugin_id = ?1",
            rusqlite::params![plugin_id],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|e| DispatchError::internal(e.to_string()))? as usize;
    if count >= STORAGE_MAX_KEYS {
        return Err(DispatchError::with_kind(
            codes::FORBIDDEN,
            "storage_quota_exceeded",
            format!("plugin storage is limited to {STORAGE_MAX_KEYS} keys"),
        ));
    }
    Ok(())
}

fn enforce_key_quota_tx(
    tx: &rusqlite::Transaction<'_>,
    plugin_id: &str,
    key: &str,
) -> Result<(), DispatchError> {
    enforce_key_quota(tx, plugin_id, key)
}

fn parse_ui_slot(params: &Value) -> Result<UiSlot, DispatchError> {
    let raw = params
        .get("slot")
        .ok_or_else(|| DispatchError::invalid_params("missing string param \"slot\""))?;
    serde_json::from_value::<UiSlot>(raw.clone())
        .map_err(|_| DispatchError::invalid_params(format!("unknown ui slot {raw}")))
}

fn ui_dispatch_error(e: UiError) -> DispatchError {
    match e {
        UiError::BadRequest(message) => DispatchError::invalid_params(message),
        UiError::QuotaExceeded => DispatchError {
            code: codes::FORBIDDEN,
            message: "plugin UI-state quota exceeded".into(),
            data: None,
        },
        UiError::StaleWorker => DispatchError {
            code: codes::FORBIDDEN,
            message: "worker generation is no longer active".into(),
            data: None,
        },
    }
}

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
            data: None,
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
    if slot == UiSlot::ComposerAction && payload.get("draft_operation").is_some() {
        ctx.require(CAP_COMPOSER_WRITE)?;
    }
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
        .notify(&ctx.plugin_id, tone, title, body, session_id, None)
        .map_err(ui_dispatch_error)?;
    Ok(json!({ "seq": seq }))
}

fn ui_open_url(
    state: &HostApiState,
    ctx: &PluginRpcContext,
    params: &Value,
) -> Result<Value, DispatchError> {
    let url = str_param(params, "url")?.to_string();
    let session_id = optional_str_param(params, "session_id")?.map(str::to_string);
    let title = optional_str_param(params, "title")?
        .map(str::to_string)
        .unwrap_or_else(|| "Open link".to_string());
    let seq = state
        .ui
        .notify(
            &ctx.plugin_id,
            Tone::Info,
            title,
            None,
            session_id,
            Some(url),
        )
        .map_err(ui_dispatch_error)?;
    Ok(json!({ "seq": seq }))
}

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
        let err = dispatch(
            &state,
            &ctx(&[]),
            "events.publish",
            &json!({"topic": "t", "payload": {}}),
        )
        .unwrap_err();
        assert_eq!(err.code, codes::FORBIDDEN);

        let err = dispatch(
            &state,
            &ctx(&[CAP_SESSION_READ]),
            "session.meta.set",
            &json!({"session_id": "s", "key": "k", "value": 1}),
        )
        .unwrap_err();
        assert_eq!(err.code, codes::FORBIDDEN);
    }

    fn ctx_for(plugin_id: &str, caps: &[&str]) -> PluginRpcContext {
        PluginRpcContext {
            plugin_id: plugin_id.to_string(),
            granted_capabilities: caps.iter().map(|c| c.to_string()).collect(),
            ui_contributions: HashSet::new(),
            ui_generation: 0,
        }
    }

    #[test]
    fn plugin_storage_roundtrip_namespace_persistence_and_cas() {
        let tmp = tempfile::tempdir().unwrap();
        let cron = ctx_for("cron", &[CAP_WORKER]);
        let other = ctx_for("other", &[CAP_WORKER]);
        let key = || json!({"key": "watermark"});
        {
            let state = state(tmp.path());
            let get = |c| dispatch(&state, c, "plugin.storage.get", &key()).unwrap();
            assert_eq!(get(&cron), json!({ "value": Value::Null }));

            dispatch(
                &state,
                &cron,
                "plugin.storage.set",
                &json!({"key": "watermark", "value": {"seq": 7}}),
            )
            .unwrap();
            assert_eq!(get(&cron), json!({ "value": {"seq": 7} }));
            assert_eq!(
                get(&other),
                json!({ "value": Value::Null }),
                "storage is namespaced per plugin"
            );

            let removed = dispatch(&state, &cron, "plugin.storage.remove", &key()).unwrap();
            assert_eq!(removed, json!({ "removed": true }));
            dispatch(
                &state,
                &cron,
                "plugin.storage.set",
                &json!({"key": "watermark", "value": "kept"}),
            )
            .unwrap();
        }
        let state = state(tmp.path());
        let got = dispatch(&state, &cron, "plugin.storage.get", &key()).unwrap();
        assert_eq!(got, json!({ "value": "kept" }), "storage survives a reopen");

        let cas = |params: Value| dispatch(&state, &cron, "plugin.storage.cas", &params);
        let cases = [
            (json!({"key": "k", "expected": null, "value": 1}), true, 1),
            (json!({"key": "k", "expected": 99, "value": 2}), false, 1),
            (json!({"key": "k", "expected": 1, "value": 2}), true, 2),
        ];
        for (params, swapped, current) in cases {
            assert_eq!(
                cas(params.clone()).unwrap(),
                json!({ "swapped": swapped, "current": current }),
                "{params}"
            );
        }
        let err = cas(json!({"key": "k", "value": 3})).unwrap_err();
        assert_eq!(err.code, codes::INVALID_PARAMS, "expected is required");
    }

    #[test]
    fn plugin_storage_enforces_quotas() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state(tmp.path());
        let c = ctx(&[CAP_WORKER]);
        let set = |key: String, value: Value| {
            dispatch(
                &state,
                &c,
                "plugin.storage.set",
                &json!({"key": key, "value": value}),
            )
        };

        let big = "x".repeat(STORAGE_MAX_VALUE_BYTES + 1);
        let err = set("k".into(), json!(big)).unwrap_err();
        assert_eq!(err.code, codes::FORBIDDEN);
        assert_eq!(err.data.as_ref().unwrap()["kind"], "storage_quota_exceeded");

        for i in 0..STORAGE_MAX_KEYS {
            set(format!("k{i}"), json!(i)).unwrap();
        }
        let err = set("overflow".into(), json!(1)).unwrap_err();
        assert_eq!(err.data.as_ref().unwrap()["kind"], "storage_quota_exceeded");
        set("k0".into(), json!("updated"))
            .expect("an existing key may still be rewritten at the key cap");
    }

    #[test]
    fn unknown_method_is_method_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state(tmp.path());
        for method in [
            "no.such",
            "config.read",
            "config.write",
            "mcp.list",
            "mcp.resolve",
            "mcp.add",
            "mcp.edit",
            "mcp.delete",
            "mcp.keep",
            "mcp.drop",
            "mcp.resolve-conflict",
            "fs.read",
            "fs.write",
            "skills.list",
            "skills.read",
            "skills.create",
            "skills.edit",
            "skills.delete",
            "skills.adopt",
            "skills.propagate",
        ] {
            let err = dispatch(&state, &ctx(&[CAP_WORKER]), method, &json!({})).unwrap_err();
            assert_eq!(err.code, codes::METHOD_NOT_FOUND, "{method}");
        }
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

    #[test]
    #[serial_test::serial]
    fn session_meta_is_namespaced_and_sessions_list_honors_dormancy() {
        use crate::session::{Instance, Storage};

        let tmp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(tmp.path());
        let now = chrono::Utc::now();
        let hour = chrono::Duration::hours(1);

        let mut rows = Vec::new();
        let mut seed = |title: &str, f: &dyn Fn(&mut Instance)| {
            let mut inst = Instance::new(title, "/tmp/plugin-host-test");
            f(&mut inst);
            let id = inst.id.clone();
            rows.push(inst);
            id
        };
        let active_id = seed("active", &|_| {});
        let archived_id = seed("archived", &|i| i.archived_at = Some(now));
        let snoozed_id = seed("future-snooze", &|i| i.snoozed_until = Some(now + hour));
        let woken_id = seed("past-snooze", &|i| i.snoozed_until = Some(now - hour));
        let trashed_id = seed("trashed", &|i| i.trashed_at = Some(now));

        let storage = Storage::new_unwatched("default").unwrap();
        storage
            .update(|instances, _groups| {
                instances.extend(rows.clone());
                Ok(())
            })
            .unwrap();

        let state =
            HostApiState::open(&tmp.path().join("plugin_events.db"), "default", 100).unwrap();
        let writer = ctx(&[CAP_SESSION_READ, CAP_SESSION_WRITE]);
        let meta = |c: &PluginRpcContext, method: &str, extra: Value| {
            let mut params = json!({"session_id": active_id, "key": "k"});
            for (k, v) in extra.as_object().expect("object").clone() {
                params[k] = v;
            }
            dispatch(&state, c, method, &params).unwrap()
        };
        meta(&writer, "session.meta.set", json!({ "value": 42 }));
        assert_eq!(
            meta(&writer, "session.meta.get", json!({}))["value"],
            json!(42)
        );
        let lose = meta(
            &writer,
            "session.meta.cas",
            json!({"expected": 0, "value": 99}),
        );
        assert_eq!(
            (&lose["swapped"], &lose["current"]),
            (&json!(false), &json!(42))
        );
        let win = meta(
            &writer,
            "session.meta.cas",
            json!({"expected": 42, "value": 99}),
        );
        assert_eq!(win["swapped"], json!(true));
        assert_eq!(
            meta(
                &ctx_for("other.plugin", &[CAP_SESSION_READ]),
                "session.meta.get",
                json!({})
            )["value"],
            json!(null),
            "session meta is namespaced per plugin"
        );

        let reader = ctx(&[CAP_SESSION_READ]);
        let list = |params: Value| dispatch(&state, &reader, "sessions.list", &params).unwrap();
        let ids = |v: &Value| -> Vec<String> {
            v["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .map(|s| s["id"].as_str().unwrap().to_string())
                .collect()
        };
        let entry = |v: &Value, id: &str| {
            v["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .find(|s| s["id"] == json!(id))
                .unwrap()
                .clone()
        };

        let all = list(json!({}));
        let all_ids = ids(&all);
        for (label, id, archived, snoozed) in [
            ("active", &active_id, false, false),
            ("archived", &archived_id, true, false),
            ("snoozed", &snoozed_id, false, true),
            ("woken", &woken_id, false, false),
        ] {
            assert!(all_ids.contains(id), "no-exclude list missing {label}");
            let row = entry(&all, id);
            assert_eq!(row["archived"], json!(archived), "{label}");
            assert_eq!(row["snoozed"], json!(snoozed), "{label}");
        }
        assert!(all_ids.contains(&trashed_id));

        let no_trash = list(json!({ "exclude": ["trashed"] }));
        let no_trash_ids = ids(&no_trash);
        assert!(!no_trash_ids.contains(&trashed_id));
        assert!(no_trash_ids.contains(&archived_id));
        assert!(no_trash_ids.contains(&active_id));
        assert_eq!(
            entry(&no_trash, &archived_id)["archived"],
            json!(true),
            "an excluded bucket must not flatten the flags of what survives"
        );

        let no_archived = ids(&list(json!({ "exclude": ["archived"] })));
        assert!(!no_archived.contains(&archived_id));
        assert!(no_archived.contains(&trashed_id));

        let live = ids(&list(
            json!({ "exclude": ["archived", "snoozed", "trashed"] }),
        ));
        assert!(live.contains(&active_id));
        assert!(live.contains(&woken_id), "an expired snooze is live again");
        for id in [&archived_id, &snoozed_id, &trashed_id] {
            assert!(!live.contains(id), "dormant {id} should be excluded");
        }

        for bad in [
            json!({ "exclude": "trashed" }),
            json!({ "exclude": ["deleted"] }),
            json!({ "exclude": [1] }),
        ] {
            let err = dispatch(&state, &reader, "sessions.list", &bad).unwrap_err();
            assert_eq!(err.code, codes::INVALID_PARAMS, "{bad}");
        }

        storage
            .update(|instances, _groups| {
                instances.retain(|i| !rows.iter().any(|seeded| seeded.id == i.id));
                Ok(())
            })
            .unwrap();
    }

    #[test]
    #[serial_test::serial]
    fn config_get_scopes_to_caller_and_requires_worker() {
        use crate::session::{update_config, PluginConfig};

        let tmp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(tmp.path());

        update_config(|config| {
            let mut plugin = PluginConfig::default();
            plugin
                .settings
                .insert("poll_interval_ms".to_string(), toml::Value::Integer(5000));
            config.plugins.insert("acme.worker".to_string(), plugin);
        })
        .unwrap();

        let state = state(tmp.path());
        let get = |c: &PluginRpcContext, key: &str| {
            dispatch(&state, c, "config.get", &json!({ "key": key }))
        };

        let worker = ctx(&[CAP_WORKER]);
        assert_eq!(
            get(&worker, "poll_interval_ms").unwrap()["value"],
            json!(5000)
        );
        assert_eq!(get(&worker, "nope").unwrap()["value"], json!(null));

        let other = PluginRpcContext {
            plugin_id: "other.plugin".to_string(),
            granted_capabilities: vec![CAP_WORKER.to_string()],
            ui_contributions: HashSet::new(),
            ui_generation: 0,
        };
        assert_eq!(
            get(&other, "poll_interval_ms").unwrap()["value"],
            json!(null),
            "settings are scoped to the calling plugin"
        );

        let err = get(&ctx(&[CAP_SESSION_READ]), "poll_interval_ms").unwrap_err();
        assert_eq!(err.code, codes::FORBIDDEN);
    }

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
    fn ui_state_set_gates_slot_capability_and_payload() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state(tmp.path());
        let set =
            |c: &PluginRpcContext, params: Value| dispatch(&state, c, "ui.state.set", &params);
        let c = ui_ctx(&state, &[CAP_WORKER], UiSlot::StatusBar, "main");

        let rejected = [
            (
                "slot the plugin did not declare",
                json!({"slot": "row-badge", "id": "main", "session_id": "s1", "payload": {"text": "x"}}),
                codes::FORBIDDEN,
            ),
            (
                "slot the host does not know",
                json!({"slot": "sidebar", "id": "main", "payload": {"text": "x"}}),
                codes::INVALID_PARAMS,
            ),
            (
                "payload missing its required text",
                json!({"slot": "status-bar", "id": "main", "payload": {"tone": "info"}}),
                codes::INVALID_PARAMS,
            ),
        ];
        for (name, params, code) in rejected {
            assert_eq!(set(&c, params).unwrap_err().code, code, "{name}");
        }

        set(
            &c,
            json!({"slot": "status-bar", "id": "main", "payload": {"text": "ok", "tone": "success"}}),
        )
        .unwrap();
        let snap = state.ui_snapshot();
        assert_eq!(snap.entries.len(), 1);
        assert_eq!(snap.entries[0].payload["text"], json!("ok"));

        let without_worker = ui_ctx(&state, &[], UiSlot::StatusBar, "main");
        let err = set(
            &without_worker,
            json!({"slot": "status-bar", "id": "main", "payload": {"text": "x"}}),
        )
        .unwrap_err();
        assert_eq!(err.code, codes::FORBIDDEN);
    }

    #[test]
    fn ui_notify_and_open_url_require_their_capabilities() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state(tmp.path());
        let open = |c: &PluginRpcContext, url: &str| {
            dispatch(&state, c, "ui.open_url", &json!({ "url": url }))
        };

        let params = json!({"title": "Build failed", "tone": "danger"});
        let c = ui_ctx(&state, &[CAP_WORKER], UiSlot::Notification, "n");
        let err = dispatch(&state, &c, "ui.notify", &params).unwrap_err();
        assert_eq!(err.code, codes::FORBIDDEN);
        assert_eq!(
            open(&c, "https://example.com").unwrap_err().code,
            codes::FORBIDDEN
        );
        let notifier = ui_ctx(&state, &[CAP_NOTIFICATIONS], UiSlot::Notification, "n");
        let ok = dispatch(&state, &notifier, "ui.notify", &params).unwrap();
        assert_eq!(ok["seq"], json!(1));

        let c = ui_ctx(&state, &[CAP_BROWSER_OPEN], UiSlot::Notification, "n");
        assert_eq!(
            open(&c, "file:///etc/passwd").unwrap_err().code,
            codes::INVALID_PARAMS
        );

        let ok = open(&c, "https://example.com/pr/1").unwrap();
        assert_eq!(ok["seq"], json!(2));
        let notifs = state.ui_snapshot().notifications;
        assert_eq!(notifs.len(), 2);
        assert_eq!(notifs[1].href.as_deref(), Some("https://example.com/pr/1"));
    }

    #[test]
    fn composer_action_draft_operation_requires_composer_write() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state(tmp.path());
        let action = |draft: bool| {
            let mut payload = json!({"label": "Voice", "method": "voice.start"});
            if draft {
                payload["draft_operation"] =
                    json!({"kind": "insert-text", "id": "op-1", "text": "hello"});
            }
            json!({
                "slot": "composer-action",
                "id": "voice",
                "session_id": "s1",
                "payload": payload
            })
        };

        let c = ui_ctx(&state, &[CAP_WORKER], UiSlot::ComposerAction, "voice");
        dispatch(&state, &c, "ui.state.set", &action(false)).unwrap();
        let err = dispatch(&state, &c, "ui.state.set", &action(true)).unwrap_err();
        assert_eq!(err.code, codes::FORBIDDEN);

        let c = ui_ctx(
            &state,
            &[CAP_WORKER, CAP_COMPOSER_WRITE],
            UiSlot::ComposerAction,
            "voice",
        );
        dispatch(&state, &c, "ui.state.set", &action(true)).unwrap();
    }
}
