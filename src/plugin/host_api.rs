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
//! - `plugin.storage.get { key }` / `set { key, value }` /
//!   `cas { key, expected, value }` / `remove { key }` (`runtime.worker`):
//!   a host-backed durable key/value store, namespaced by the calling
//!   plugin's id (#2897). Survives daemon and worker restarts; quota-bounded.
//! - `fs.read { root, path }` (`fs.read`) and `fs.write { root, path, content }`
//!   (`fs.write`): a UTF-8 file under one of two AoE-owned roots, `plugin` (the
//!   caller's private `<app_dir>/plugins/<id>/files`) or `skills` (the managed
//!   `<app_dir>/skills` store). Confined to the root; no arbitrary host path.
//! - `skills.list` / `skills.read { source, directory }` (`fs.read` OR
//!   `config.read`): the discovered skill set, source-qualified by provenance.
//! - `skills.create` / `skills.edit` / `skills.delete` (`fs.write`): mutate the
//!   AoE-managed skills store in place. `skills.adopt` (`fs.write`) copies a
//!   host-discovered skill INTO the managed store, leaving the original.
//!   `skills.propagate` (`fs.write`) copies a managed skill OUT into a supported
//!   agent's host skills dir. A host-discovered (read-only) skill target for a
//!   create/edit/delete is refused with `FORBIDDEN`.
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
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

use anyhow::Context as _;
use aoe_plugin_api::UiSlot;
use rusqlite::{Connection, OptionalExtension};
use serde_json::{json, Value};

use crate::events::{self, Order, Schema, SeqBound};
use crate::plugin::protocol::codes;
use crate::plugin::ui_state::{Tone, UiError, UiSnapshot, UiStore};
use crate::session::mcp_model::{self, McpProvenance};
use crate::session::mcp_state::{self, ConflictWinner, ResolveStatus};
use crate::session::settings_schema::{self, Scope, WebWritePolicy};
use crate::session::{mcp_overrides, update_config, Storage};

/// Capability required by each host method. Reused from the manifest taxonomy
/// (`aoe_plugin_api::KNOWN_CAPABILITIES`); no new capability is introduced.
const CAP_WORKER: &str = "runtime.worker";
const CAP_SESSION_READ: &str = "session.read";
const CAP_SESSION_WRITE: &str = "session.write";
/// `ui.notify` posts a notification; it reuses the existing `notifications`
/// capability. `ui.state.*` need no extra capability beyond `runtime.worker`:
/// the gate is the manifest `ui` slot declaration (see [`PluginRpcContext`]).
const CAP_NOTIFICATIONS: &str = "notifications";
const CAP_COMPOSER_WRITE: &str = "composer.write";
const CAP_BROWSER_OPEN: &str = "browser_open";
/// Host/global (not own-table) config: `config.read` gates reading a settings
/// field and resolving the MCP surface; `config.write` gates every host-config
/// and MCP mutation. Distinct from `config.get` (`runtime.worker`), which reads
/// the calling plugin's own settings only.
const CAP_CONFIG_READ: &str = "config.read";
const CAP_CONFIG_WRITE: &str = "config.write";

/// Plugin-private storage quotas (#2897), per plugin. A plugin cannot reach
/// another plugin's namespace, so the store needs no user-facing capability;
/// these caps bound one plugin's footprint. Config exposure is a follow-up.
const STORAGE_MAX_KEYS: usize = 64;
const STORAGE_MAX_KEY_BYTES: usize = 256;
const STORAGE_MAX_VALUE_BYTES: usize = 64 * 1024;

/// The declared-but-previously-unwired filesystem capabilities (#2984). `fs.*`
/// and the skills write RPCs gate on these; skills reads accept `fs.read` or
/// `config.read` (declared above).
const CAP_FS_READ: &str = "fs.read";
const CAP_FS_WRITE: &str = "fs.write";

/// Cap a single `fs.*` read/write and a `SKILL.md` body at 1 MiB, so a plugin
/// cannot make the host buffer an unbounded blob over the worker protocol.
const MAX_FS_BYTES: u64 = 1024 * 1024;

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
    /// Monotonic settings revision, bumped on every settings write (#2897).
    /// `config.get` returns it so a worker can tell whether a fetch already
    /// reflects a `plugin.settings.changed` event it received. In-memory: a
    /// worker re-reads config on restart anyway, so cross-restart durability
    /// buys nothing.
    settings_revision: std::sync::atomic::AtomicU64,
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
        // Plugin-private KV store (#2897): host-backed, namespaced by plugin
        // id, lives alongside the event bus in the app dir (never the install
        // dir, which an upgrade can replace). Retained on uninstall like
        // `plugin_meta`, since it is cheap and reinstalling restores state.
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

    /// Bump and return the settings revision. Called by the settings write
    /// path so the next `config.get` reflects the change.
    pub fn bump_settings_revision(&self) -> u64 {
        // Release so a reader that observes the new revision with Acquire also
        // sees the settings write that preceded the bump.
        self.settings_revision
            .fetch_add(1, std::sync::atomic::Ordering::Release)
            + 1
    }

    fn settings_revision(&self) -> u64 {
        self.settings_revision
            .load(std::sync::atomic::Ordering::Acquire)
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
        let _ = self.ui.notify(plugin_id, tone, title, body, None, None);
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
    /// Refuse the call unless the plugin holds `capability`. Shared with the
    /// async session RPC module, hence pub(crate).
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

    /// Refuse the call unless the plugin holds at least one of `capabilities`.
    /// Used where either of two grants suffices (e.g. `skills.list` accepts
    /// `fs.read` OR `config.read`).
    fn require_any(&self, capabilities: &[&str]) -> Result<(), DispatchError> {
        if capabilities
            .iter()
            .any(|cap| self.granted_capabilities.iter().any(|c| c == cap))
        {
            Ok(())
        } else {
            Err(DispatchError {
                code: codes::FORBIDDEN,
                message: format!(
                    "plugin {} holds none of the required capabilities {capabilities:?}",
                    self.plugin_id
                ),
                data: Some(serde_json::json!({
                    "kind": "capability_missing",
                    "required_capabilities": capabilities,
                })),
            })
        }
    }
}

/// A failed dispatch, carrying the JSON-RPC error code, diagnostic message,
/// and optional structured `data` (whose `kind` field is the stable
/// machine-readable contract) to return.
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
    fn forbidden(msg: impl Into<String>) -> Self {
        Self {
            code: codes::FORBIDDEN,
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

    /// An error whose `data.kind` is part of the stable plugin API (#2897).
    pub(crate) fn with_kind(code: i64, kind: &str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: Some(serde_json::json!({ "kind": kind })),
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
        // Plugin-private storage (#2897). Namespaced by ctx.plugin_id only, so
        // one plugin cannot reach another's keys; no user-facing capability
        // beyond runtime.worker (which every worker holds), mirroring
        // config.get's rationale.
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
        "config.read" => {
            ctx.require(CAP_CONFIG_READ)?;
            config_read(params)
        }
        "config.write" => {
            ctx.require(CAP_CONFIG_WRITE)?;
            config_write(params)
        }
        "mcp.list" => {
            ctx.require(CAP_CONFIG_READ)?;
            mcp_list(state, params)
        }
        "mcp.resolve" => {
            ctx.require(CAP_CONFIG_READ)?;
            mcp_resolve(state, params)
        }
        "mcp.add" => {
            ctx.require(CAP_CONFIG_WRITE)?;
            mcp_add(state, params)
        }
        "mcp.edit" => {
            ctx.require(CAP_CONFIG_WRITE)?;
            mcp_edit(state, params)
        }
        "mcp.delete" => {
            ctx.require(CAP_CONFIG_WRITE)?;
            mcp_delete(state, params)
        }
        "mcp.keep" => {
            ctx.require(CAP_CONFIG_WRITE)?;
            mcp_keep(state, params)
        }
        "mcp.drop" => {
            ctx.require(CAP_CONFIG_WRITE)?;
            mcp_drop(state, params)
        }
        "mcp.resolve-conflict" => {
            ctx.require(CAP_CONFIG_WRITE)?;
            mcp_resolve_conflict(state, params)
        }
        // Filesystem and skills RPCs (#2984). Every mutating arm below inherits
        // read-only safety for free: the plugin host is never spawned in
        // read-only serve mode (`src/server/mod.rs` gates `PluginHost::new` on
        // `!read_only`), so no per-arm read-only check is needed here.
        "fs.read" => {
            ctx.require(CAP_FS_READ)?;
            fs_read(ctx, params)
        }
        "fs.write" => {
            ctx.require(CAP_FS_WRITE)?;
            fs_write(ctx, params)
        }
        "skills.list" => {
            ctx.require_any(&[CAP_FS_READ, CAP_CONFIG_READ])?;
            skills_list()
        }
        "skills.read" => {
            ctx.require_any(&[CAP_FS_READ, CAP_CONFIG_READ])?;
            skills_read(params)
        }
        "skills.create" => {
            ctx.require(CAP_FS_WRITE)?;
            skills_create(params)
        }
        "skills.edit" => {
            ctx.require(CAP_FS_WRITE)?;
            skills_edit(params)
        }
        "skills.delete" => {
            ctx.require(CAP_FS_WRITE)?;
            skills_delete(params)
        }
        "skills.adopt" => {
            ctx.require(CAP_FS_WRITE)?;
            skills_adopt(params)
        }
        "skills.propagate" => {
            ctx.require(CAP_FS_WRITE)?;
            skills_propagate(params)
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
fn config_get(
    state: &HostApiState,
    ctx: &PluginRpcContext,
    params: &Value,
) -> Result<Value, DispatchError> {
    let key = str_param(params, "key")?;
    // Read the revision before and after loading so a settings write that lands
    // mid-load cannot pair a stale value with the new revision; retry when a
    // bump slips in between (#2897). A worker reacting to a
    // `plugin.settings.changed` event uses the returned revision to tell whether
    // this fetch already reflects it, so the pair must be consistent.
    // ponytail: settings writes are rare human actions, so a few retries is
    // ample; the bound just stops a pathological write storm from spinning.
    let mut value = Value::Null;
    let mut revision = state.settings_revision();
    for _ in 0..8 {
        let rev_before = revision;
        let config =
            crate::session::Config::load().map_err(|e| DispatchError::internal(e.to_string()))?;
        value = match config
            .plugins
            .get(&ctx.plugin_id)
            .and_then(|plugin| plugin.settings.get(key))
        {
            // The stored value is TOML; hand it back to the worker as JSON.
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

/// Validate a storage key: non-empty and within the byte cap. The key is
/// caller input, so a bad one is INVALID_PARAMS, not a host failure.
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

/// Serialize a storage value and enforce the size cap.
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
    // `expected` is required: defaulting an omitted field to null would turn a
    // malformed request into a silent create-if-absent. A caller wanting that
    // passes `expected: null` explicitly.
    let expected = params
        .get("expected")
        .cloned()
        .ok_or_else(|| DispatchError::invalid_params("missing param \"expected\""))?;
    let now = chrono::Utc::now().timestamp_millis();
    let mut conn = state.events.lock().unwrap_or_else(|p| p.into_inner());
    // One transaction so the read-compare-write cannot interleave with
    // another worker task's storage call.
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

/// Decode a stored value_json cell into JSON. A corrupt row is a host bug,
/// not caller input.
fn decode_stored(stored: Option<String>) -> Result<Value, DispatchError> {
    match stored {
        Some(json) => {
            serde_json::from_str(&json).map_err(|e| DispatchError::internal(e.to_string()))
        }
        None => Ok(Value::Null),
    }
}

/// Refuse a set/cas that would create a NEW key past the per-plugin key cap.
/// Overwriting an existing key is always allowed.
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

/// Same key-count quota check inside an open transaction.
fn enforce_key_quota_tx(
    tx: &rusqlite::Transaction<'_>,
    plugin_id: &str,
    key: &str,
) -> Result<(), DispatchError> {
    enforce_key_quota(tx, plugin_id, key)
}

/// Gate a `(section, field)` to the non-elevated host-config surface shared by
/// `config.read` / `config.write`: the field must be a known schema descriptor
/// a non-elevated web client may also write (`WebWritePolicy::Allow`). An
/// unknown field is `INVALID_PARAMS`; a host-execution (`local_only`) or
/// elevation-gated field is `FORBIDDEN`. The elevation-gated set can carry
/// literal secrets (e.g. `sandbox.environment` env values), so it is off-limits
/// for reads too, not just writes. `verb` (`"readable"` / `"writable"`) tailors
/// the message.
fn require_non_elevated_field(section: &str, field: &str, verb: &str) -> Result<(), DispatchError> {
    match settings_schema::descriptor(section, field) {
        None => Err(DispatchError::invalid_params(format!(
            "unknown config field {section}.{field}"
        ))),
        Some(d) => match d.web_write {
            WebWritePolicy::Allow => Ok(()),
            WebWritePolicy::LocalOnly { .. } => Err(DispatchError::forbidden(format!(
                "config field {section}.{field} is a host-execution surface and is not {verb} by plugins"
            ))),
            WebWritePolicy::RequiresElevation { .. } => Err(DispatchError::forbidden(format!(
                "config field {section}.{field} is elevation-gated and is not {verb} by plugins"
            ))),
        },
    }
}

/// Read one host/global settings field (`config.read`, cap `config.read`). The
/// `(section, field)` pair must be a plain (non-elevated) schema descriptor, so
/// a plugin can only read declared, non-secret settings: an unknown field is
/// `INVALID_PARAMS`, and a host-execution (`local_only`) or elevation-gated
/// field, which can carry literal secrets, is `FORBIDDEN` (symmetric with
/// `config.write`). Returns the value from the serialized global `Config`, or
/// `null` when the field is unset/omitted. Distinct from `config.get`, which
/// reads the caller's own plugin settings.
fn config_read(params: &Value) -> Result<Value, DispatchError> {
    let section = str_param(params, "section")?;
    let field = str_param(params, "field")?;
    require_non_elevated_field(section, field, "readable")?;
    let config =
        crate::session::Config::load().map_err(|e| DispatchError::internal(e.to_string()))?;
    let json = serde_json::to_value(&config).map_err(|e| DispatchError::internal(e.to_string()))?;
    let value = json
        .get(section)
        .and_then(|s| s.get(field))
        .cloned()
        .unwrap_or(Value::Null);
    Ok(json!({ "value": value }))
}

/// Write host/global settings (`config.write`, cap `config.write`). The `patch`
/// is the web-PATCH shape `{ section: { field: value } }`. A plugin gets exactly
/// the NON-elevated web write surface, but unlike the web path (which silently
/// strips `local_only` leaves) an RPC rejects every disallowed leaf loudly so a
/// plugin never believes a refused write landed: unknown field -> INVALID_PARAMS;
/// host-execution (`local_only`) or elevation-required field -> FORBIDDEN. The
/// value itself is validated through the shared schema gate.
fn config_write(params: &Value) -> Result<Value, DispatchError> {
    let patch = params
        .get("patch")
        .ok_or_else(|| DispatchError::invalid_params("missing object param \"patch\""))?;
    let sections = patch.as_object().ok_or_else(|| {
        DispatchError::invalid_params(
            "\"patch\" must be an object of { section: { field: value } }",
        )
    })?;
    if sections.is_empty() {
        return Err(DispatchError::invalid_params("\"patch\" is empty"));
    }
    for (section, fields) in sections {
        let fields = fields.as_object().ok_or_else(|| {
            DispatchError::invalid_params(format!("patch section {section:?} must be an object"))
        })?;
        for field in fields.keys() {
            require_non_elevated_field(section, field, "writable")?;
        }
    }
    // Value validation via the shared gate. `require_non_elevated_field` above
    // already rejected unknown / local_only / elevation fields; this pass checks
    // each value against its schema rule. `elevated = false` re-guards the
    // elevation policy, but `validate_patch` does NOT reject `local_only` (it
    // expects the web caller to have stripped it first), so the loud rejection
    // above is what keeps a host-execution leaf out.
    settings_schema::validate_patch(patch, Scope::Global, false)
        .map_err(|rej| DispatchError::invalid_params(rej.message()))?;

    let patch = patch.clone();
    update_config(|config| -> anyhow::Result<()> {
        let mut current = serde_json::to_value(&*config)?;
        settings_schema::merge_json(&mut current, &patch);
        *config = serde_json::from_value(current)?;
        Ok(())
    })
    .and_then(|inner| inner)
    .map_err(|e| DispatchError::internal(e.to_string()))?;
    Ok(json!({ "ok": true }))
}

/// Resolve the `(agent, profile, cwd)` an MCP call operates in. `agent` is the
/// optional `agent` param, else the host profile's configured default tool, else
/// `claude` (mirrors the REST surface). `profile` is the host's profile; `cwd`
/// is the daemon working directory (from which the project-local layer resolves).
fn mcp_context(
    state: &HostApiState,
    params: &Value,
) -> Result<(String, Option<String>, PathBuf), DispatchError> {
    let profile = state.profile.clone();
    let agent = match optional_str_param(params, "agent")? {
        Some(a) => a.to_string(),
        None => crate::session::profile_config::resolve_config_or_warn(&profile)
            .session
            .default_tool
            .unwrap_or_else(|| "claude".to_string()),
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let profile_opt = (!profile.is_empty()).then_some(profile);
    Ok((agent, profile_opt, cwd))
}

/// Wrap the `{ name, ...ecosystem .mcp.json entry }` params into a one-server
/// standard config and parse it via the shared ecosystem parser, so a plugin
/// sends the exact `.mcp.json` shape users and other agents already use. A
/// missing/empty name or a malformed transport is `INVALID_PARAMS`.
fn parse_mcp_server_param(
    params: &Value,
) -> Result<crate::session::project_mcp::ProjectMcpServer, DispatchError> {
    let name = str_param(params, "name")?;
    if name.trim().is_empty() {
        return Err(DispatchError::invalid_params(
            "MCP server \"name\" must be non-empty",
        ));
    }
    let mut entry = params
        .as_object()
        .ok_or_else(|| DispatchError::invalid_params("params must be an object"))?
        .clone();
    // `name` is the map key, not an entry field; drop it so it is not treated as
    // an (ignored) server property.
    entry.remove("name");
    let wrapped = json!({ "mcpServers": { name: Value::Object(entry) } });
    let text =
        serde_json::to_string(&wrapped).map_err(|e| DispatchError::internal(e.to_string()))?;
    let mut servers =
        crate::session::project_mcp::parse_standard_mcp_servers(&text).map_err(|e| {
            DispatchError::invalid_params(format!("invalid MCP server definition: {e}"))
        })?;
    servers
        .pop()
        .ok_or_else(|| DispatchError::internal("MCP parser returned no server"))
}

/// `mcp.list` (cap `config.read`): the pure, redacted effective forwarded set.
/// Uses `resolve_effective` (no drift reconcile, no state write), unlike
/// `mcp.resolve`.
fn mcp_list(state: &HostApiState, params: &Value) -> Result<Value, DispatchError> {
    let (agent, profile, cwd) = mcp_context(state, params)?;
    let effective = mcp_model::resolve_effective(&agent, profile.as_deref(), &cwd);
    Ok(json!({
        "agent": agent,
        "servers": effective.iter().map(|s| s.redacted()).collect::<Vec<_>>(),
    }))
}

/// `mcp.resolve` (cap `config.read`): the full management surface (effective set,
/// kept-on-removal, conflicts, drift-paused), redacted. Mirrors the REST
/// `GET /api/mcp/servers`; note this reconciles the drift snapshot as a side
/// effect (adopts newly seen native servers), which the pure `mcp.list` does not.
fn mcp_resolve(state: &HostApiState, params: &Value) -> Result<Value, DispatchError> {
    let (agent, profile, cwd) = mcp_context(state, params)?;
    let view = mcp_model::resolve_surface(&agent, profile.as_deref(), &cwd);
    Ok(json!({
        "agent": agent,
        "effective": view.effective.iter().map(|s| s.redacted()).collect::<Vec<_>>(),
        "keptOnRemoval": view.kept_on_removal.iter().map(|s| s.redacted()).collect::<Vec<_>>(),
        "conflicts": view
            .conflicts
            .iter()
            .map(|c| json!({
                "name": c.current.name,
                "agent": c.agent,
                "previous": c.previous.redacted_summary(),
                "current": c.current.redacted_summary(),
                "fingerprint": c.fingerprint(),
            }))
            .collect::<Vec<_>>(),
        "driftPaused": view.drift_paused,
    }))
}

/// True if `name` resolves in the effective set from a layer other than the
/// AoE-owned global one (agent-native / profile / project-local). Such a name is
/// not AoE's to write, so `mcp.add` / `mcp.edit` / `mcp.delete` reject it with
/// `FORBIDDEN`. A pure read (`resolve_effective`, no drift write).
fn resolves_non_global(
    state: &HostApiState,
    params: &Value,
    name: &str,
) -> Result<bool, DispatchError> {
    let (agent, profile, cwd) = mcp_context(state, params)?;
    let effective = mcp_model::resolve_effective(&agent, profile.as_deref(), &cwd);
    Ok(effective
        .iter()
        .any(|s| s.def.name == name && s.provenance != McpProvenance::Global))
}

fn not_global_forbidden(name: &str) -> DispatchError {
    DispatchError::forbidden(format!(
        "MCP server {name:?} is owned by a non-global layer (agent-native, profile, or project-local); AoE only writes the global layer"
    ))
}

/// `mcp.add` (cap `config.write`): create a new server in the global `mcp.json`.
/// A name owned by a non-global layer is `FORBIDDEN` (AoE will not add a global
/// override that shadows it); a name that already exists globally is
/// `INVALID_PARAMS` (the caller uses `mcp.edit`). The global existence check is
/// atomic under the file lock.
fn mcp_add(state: &HostApiState, params: &Value) -> Result<Value, DispatchError> {
    let server = parse_mcp_server_param(params)?;
    if resolves_non_global(state, params, &server.name)? {
        return Err(not_global_forbidden(&server.name));
    }
    let created = mcp_overrides::insert_global_server_if_absent(&server)
        .map_err(|e| DispatchError::internal(e.to_string()))?;
    if !created {
        return Err(DispatchError::invalid_params(format!(
            "global MCP server {:?} already exists; use mcp.edit",
            server.name
        )));
    }
    Ok(json!({ "status": "added" }))
}

/// `mcp.edit` (cap `config.write`): replace an existing global server definition.
/// A full replacement: fields omitted from the entry (including env / header
/// secrets) are dropped, matching the global `upsert` semantics. A name owned by
/// a non-global layer is `FORBIDDEN`; a name that exists nowhere globally is
/// `INVALID_PARAMS` (the caller uses `mcp.add`).
fn mcp_edit(state: &HostApiState, params: &Value) -> Result<Value, DispatchError> {
    let server = parse_mcp_server_param(params)?;
    let replaced = mcp_overrides::replace_global_server_if_present(&server)
        .map_err(|e| DispatchError::internal(e.to_string()))?;
    if replaced {
        return Ok(json!({ "status": "edited" }));
    }
    if resolves_non_global(state, params, &server.name)? {
        return Err(not_global_forbidden(&server.name));
    }
    Err(DispatchError::invalid_params(format!(
        "no global MCP server {:?}; use mcp.add",
        server.name
    )))
}

/// `mcp.delete` (cap `config.write`): remove a server from the global `mcp.json`.
/// Only the AoE-owned global layer is writable: a name that resolves from an
/// agent-native / profile / project-local layer is `FORBIDDEN` (AoE never writes
/// those files); a name present nowhere is `INVALID_PARAMS`.
fn mcp_delete(state: &HostApiState, params: &Value) -> Result<Value, DispatchError> {
    let name = str_param(params, "name")?.to_string();
    let removed = mcp_overrides::remove_global_server(&name)
        .map_err(|e| DispatchError::internal(e.to_string()))?;
    if removed {
        return Ok(json!({ "status": "deleted" }));
    }
    // Not in the global layer. Classify for a precise error: a non-global
    // provenance is a forbidden target, anything else is simply not found.
    if resolves_non_global(state, params, &name)? {
        Err(not_global_forbidden(&name))
    } else {
        Err(DispatchError::invalid_params(format!(
            "unknown global MCP server {name:?}"
        )))
    }
}

/// `mcp.keep` (cap `config.write`): keep a server removed from a native config by
/// promoting it into the global `mcp.json` (feature D). `INVALID_PARAMS` if no
/// such kept-on-removal entry exists.
fn mcp_keep(state: &HostApiState, params: &Value) -> Result<Value, DispatchError> {
    let (agent, _profile, _cwd) = mcp_context(state, params)?;
    let name = str_param(params, "name")?;
    let kept = mcp_state::keep_removed(&agent, name)
        .map_err(|e| DispatchError::internal(e.to_string()))?;
    if kept {
        Ok(json!({ "status": "kept" }))
    } else {
        Err(DispatchError::invalid_params(format!(
            "no kept-on-removal MCP server {name:?} for agent {agent:?}"
        )))
    }
}

/// `mcp.drop` (cap `config.write`): drop a kept-on-removal server without
/// promoting it (feature D). Idempotent: a name already gone still returns ok.
fn mcp_drop(state: &HostApiState, params: &Value) -> Result<Value, DispatchError> {
    let (agent, _profile, _cwd) = mcp_context(state, params)?;
    let name = str_param(params, "name")?;
    mcp_state::forget_native(&agent, name).map_err(|e| DispatchError::internal(e.to_string()))?;
    Ok(json!({ "status": "dropped" }))
}

/// `mcp.resolve-conflict` (cap `config.write`): resolve a drift conflict for one
/// server (feature C). Mirrors the REST resolve endpoint: re-resolve the current
/// conflicts, find the one for `name`, and apply `winner` (`aoe` / `native`)
/// under the `fingerprint` optimistic-concurrency token. A stale token or a
/// conflict that no longer exists returns `{ status: "stale" }`.
fn mcp_resolve_conflict(state: &HostApiState, params: &Value) -> Result<Value, DispatchError> {
    let (agent, _profile, _cwd) = mcp_context(state, params)?;
    let name = str_param(params, "name")?.to_string();
    let winner = match str_param(params, "winner")? {
        "aoe" => ConflictWinner::Aoe,
        "native" => ConflictWinner::Native,
        other => {
            return Err(DispatchError::invalid_params(format!(
                "unknown winner {other:?} (expected \"aoe\" or \"native\")"
            )))
        }
    };
    let fingerprint = str_param(params, "fingerprint")?.to_string();

    let read = mcp_model::load_native_mcp_servers_checked_from_home(&agent)
        .map_err(|e| DispatchError::internal(e.to_string()))?;
    let reconcile = mcp_state::reconcile_agent(&agent, &read)
        .map_err(|e| DispatchError::internal(e.to_string()))?;
    let Some(conflict) = reconcile
        .conflicts
        .into_iter()
        .find(|c| c.current.name == name)
    else {
        return Ok(json!({ "status": "stale" }));
    };
    match mcp_state::resolve_conflict(&conflict, winner, &fingerprint)
        .map_err(|e| DispatchError::internal(e.to_string()))?
    {
        ResolveStatus::Applied => Ok(json!({ "status": "applied" })),
        ResolveStatus::Stale => Ok(json!({ "status": "stale" })),
    }
}

/// The two roots the `fs.*` RPCs may touch. Both are AoE-owned dirs under the
/// app dir; a host-discovered agent skills dir (`~/.claude/skills`, ...) is
/// deliberately NOT reachable, so `fs.write` can never mutate a read-only host
/// skill. That path exists only through the constrained `skills.propagate`.
enum FsRoot {
    /// The calling plugin's private storage, `<app_dir>/plugins/<id>/files`.
    /// Scoped to the caller's own id, never a request parameter.
    Plugin,
    /// The AoE-managed skills store, `<app_dir>/skills`.
    Skills,
}

/// Resolve the requested `root` param to an [`FsRoot`].
fn parse_fs_root(params: &Value) -> Result<FsRoot, DispatchError> {
    match str_param(params, "root")? {
        "plugin" => Ok(FsRoot::Plugin),
        "skills" => Ok(FsRoot::Skills),
        other => Err(DispatchError::invalid_params(format!(
            "unknown fs root {other:?}; expected \"plugin\" or \"skills\""
        ))),
    }
}

/// The on-disk directory for an [`FsRoot`]. The plugin root is namespaced to the
/// caller's own id.
fn fs_root_dir(ctx: &PluginRpcContext, root: FsRoot) -> Result<PathBuf, DispatchError> {
    let app_dir =
        crate::session::get_app_dir().map_err(|e| DispatchError::internal(e.to_string()))?;
    Ok(match root {
        FsRoot::Plugin => app_dir.join("plugins").join(&ctx.plugin_id).join("files"),
        FsRoot::Skills => app_dir.join("skills"),
    })
}

/// Join `rel` onto `root` after a lexical containment check: `rel` must be
/// relative and contain only normal / current-dir components. This rejects
/// `..`, absolute paths, and Windows prefixes before any filesystem access.
fn confine_lexical(root: &Path, rel: &str) -> Result<PathBuf, DispatchError> {
    if rel.is_empty() {
        return Err(DispatchError::invalid_params("param \"path\" is empty"));
    }
    let rel_path = Path::new(rel);
    if rel_path.is_absolute() {
        return Err(DispatchError::invalid_params("path must be relative"));
    }
    for comp in rel_path.components() {
        match comp {
            Component::Normal(_) | Component::CurDir => {}
            _ => {
                return Err(DispatchError::invalid_params(
                    "path may not contain \"..\" or absolute/prefix components",
                ))
            }
        }
    }
    Ok(root.join(rel_path))
}

/// After the lexical guard, confirm `path` (which must exist) canonicalizes to
/// somewhere under `root`, catching an ancestor symlink that redirects out of
/// the root. This is the honest-cooperative bound (a TOCTOU race remains between
/// this check and the open); the plugin host is a cooperative-plugin boundary,
/// not adversarial containment.
fn ensure_under_root(root: &Path, path: &Path) -> Result<(), DispatchError> {
    let canon_root =
        std::fs::canonicalize(root).map_err(|e| DispatchError::internal(e.to_string()))?;
    let canon_path =
        std::fs::canonicalize(path).map_err(|e| DispatchError::internal(e.to_string()))?;
    if canon_path.starts_with(&canon_root) {
        Ok(())
    } else {
        Err(DispatchError::with_kind(
            codes::FORBIDDEN,
            "path_escapes_root",
            "path escapes its root",
        ))
    }
}

/// Create every missing directory from `root` down to `target`, refusing to
/// create through or follow a symlink component. Unlike a bare
/// `create_dir_all` followed by a containment check, this validates each
/// component before the mkdir, so a pre-existing symlink inside the root can
/// never make `fs.write` materialize a directory outside it.
fn create_dirs_confined(root: &Path, target: &Path) -> Result<(), DispatchError> {
    std::fs::create_dir_all(root).map_err(|e| DispatchError::internal(e.to_string()))?;
    let rel = target.strip_prefix(root).map_err(|_| {
        DispatchError::with_kind(
            codes::FORBIDDEN,
            "path_escapes_root",
            "path escapes its root",
        )
    })?;
    let mut cur = root.to_path_buf();
    for comp in rel.components() {
        match comp {
            Component::Normal(c) => cur.push(c),
            Component::CurDir => continue,
            _ => {
                return Err(DispatchError::invalid_params(
                    "path may not contain \"..\" or absolute/prefix components",
                ))
            }
        }
        match std::fs::symlink_metadata(&cur) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(DispatchError::with_kind(
                    codes::FORBIDDEN,
                    "is_symlink",
                    "refusing to create through a symlink",
                ))
            }
            Ok(m) if m.is_dir() => {}
            Ok(_) => {
                return Err(DispatchError::invalid_params(
                    "path component is not a directory",
                ))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&cur).map_err(|e| DispatchError::internal(e.to_string()))?;
            }
            Err(e) => return Err(DispatchError::internal(e.to_string())),
        }
    }
    Ok(())
}

/// Read a UTF-8 file from one of the two `fs.*` roots. Refuses symlinks and
/// non-regular files; bounded by [`MAX_FS_BYTES`].
fn fs_read(ctx: &PluginRpcContext, params: &Value) -> Result<Value, DispatchError> {
    let root = fs_root_dir(ctx, parse_fs_root(params)?)?;
    let rel = str_param(params, "path")?;
    let target = confine_lexical(&root, rel)?;
    match std::fs::symlink_metadata(&target) {
        Ok(m) if m.file_type().is_symlink() => {
            return Err(DispatchError::with_kind(
                codes::FORBIDDEN,
                "is_symlink",
                "refusing to read a symlink",
            ))
        }
        Ok(m) if !m.is_file() => {
            return Err(DispatchError::invalid_params("path is not a regular file"))
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(DispatchError::invalid_params("no such file"))
        }
        Err(e) => return Err(DispatchError::internal(e.to_string())),
    }
    ensure_under_root(&root, &target)?;
    // Bounded single-handle read so a file that grows after the metadata check
    // above cannot exceed the cap (the metadata-then-read TOCTOU).
    let content = crate::session::skills_model::read_file_capped(&target, MAX_FS_BYTES)
        .map_err(|e| DispatchError::invalid_params(e.to_string()))?;
    Ok(json!({ "content": content }))
}

/// Write a UTF-8 file into one of the two `fs.*` roots, creating parent dirs.
/// Refuses to overwrite through a symlink; bounded by [`MAX_FS_BYTES`].
fn fs_write(ctx: &PluginRpcContext, params: &Value) -> Result<Value, DispatchError> {
    let root = fs_root_dir(ctx, parse_fs_root(params)?)?;
    let rel = str_param(params, "path")?;
    let content = str_param(params, "content")?;
    if content.len() as u64 > MAX_FS_BYTES {
        return Err(DispatchError::invalid_params("content is too large"));
    }
    let target = confine_lexical(&root, rel)?;
    // Create parent dirs component-by-component, validating containment BEFORE
    // each mkdir, so a symlink component cannot make us materialize a directory
    // outside the root before a later containment check would reject it.
    let parent = target.parent().unwrap_or(&root);
    create_dirs_confined(&root, parent)?;
    if let Ok(m) = std::fs::symlink_metadata(&target) {
        if m.file_type().is_symlink() {
            return Err(DispatchError::with_kind(
                codes::FORBIDDEN,
                "is_symlink",
                "refusing to overwrite a symlink",
            ));
        }
    }
    std::fs::write(&target, content).map_err(|e| DispatchError::internal(e.to_string()))?;
    Ok(json!({ "ok": true }))
}

/// Resolve `(home, app_dir)` for the skills-store operations.
fn skills_dirs() -> Result<(PathBuf, PathBuf), DispatchError> {
    let home =
        dirs::home_dir().ok_or_else(|| DispatchError::internal("could not resolve home dir"))?;
    let app_dir =
        crate::session::get_app_dir().map_err(|e| DispatchError::internal(e.to_string()))?;
    Ok((home, app_dir))
}

/// Map a [`skills_model::SkillError`](crate::session::skills_model::SkillError)
/// to a JSON-RPC error: a read-only target is `FORBIDDEN`, an I/O failure is
/// `INTERNAL_ERROR`, everything else is caller input (`INVALID_PARAMS`).
fn skill_dispatch_error(e: crate::session::skills_model::SkillError) -> DispatchError {
    use crate::session::skills_model::SkillError as E;
    match e {
        E::InvalidInput(m) => DispatchError::invalid_params(m),
        E::NotFound(m) => DispatchError::invalid_params(format!("skill not found: {m}")),
        E::Collision(m) => DispatchError::invalid_params(format!("skill already exists: {m}")),
        E::ReadOnly(m) => DispatchError::with_kind(codes::FORBIDDEN, "read_only", m),
        E::Io(err) => DispatchError::internal(err.to_string()),
    }
}

/// Parse the source-qualified `source` param into a
/// [`SkillProvenance`](crate::session::skills_model::SkillProvenance).
fn parse_skill_source(
    params: &Value,
) -> Result<crate::session::skills_model::SkillProvenance, DispatchError> {
    let raw = params
        .get("source")
        .ok_or_else(|| DispatchError::invalid_params("missing param \"source\""))?;
    serde_json::from_value(raw.clone())
        .map_err(|e| DispatchError::invalid_params(format!("invalid \"source\": {e}")))
}

/// JSON view of one discovered/read skill's provenance.
fn provenance_json(
    p: &crate::session::skills_model::SkillProvenance,
) -> Result<Value, DispatchError> {
    serde_json::to_value(p).map_err(|e| DispatchError::internal(e.to_string()))
}

fn skills_list() -> Result<Value, DispatchError> {
    let skills = crate::session::skills_model::discover_all()
        .map_err(|e| DispatchError::internal(e.to_string()))?;
    let out = skills
        .iter()
        .map(|s| {
            Ok(json!({
                "directory": s.directory,
                "name": s.name,
                "description": s.description,
                "provenance": provenance_json(&s.provenance)?,
                "provenance_label": s.provenance.label(),
                "writable": s.provenance.is_writable(),
            }))
        })
        .collect::<Result<Vec<_>, DispatchError>>()?;
    Ok(json!({ "skills": out }))
}

fn skills_read(params: &Value) -> Result<Value, DispatchError> {
    let source = parse_skill_source(params)?;
    let directory = str_param(params, "directory")?;
    let (home, app_dir) = skills_dirs()?;
    let read = crate::session::skills_model::read_skill(&home, &app_dir, &source, directory)
        .map_err(skill_dispatch_error)?;
    Ok(json!({
        "directory": read.directory,
        "name": read.name,
        "description": read.description,
        "content": read.content,
        "provenance": provenance_json(&read.provenance)?,
    }))
}

fn skills_create(params: &Value) -> Result<Value, DispatchError> {
    let directory = str_param(params, "directory")?;
    let description = optional_str_param(params, "description")?;
    let (_home, app_dir) = skills_dirs()?;
    crate::session::skills_model::create_skill(&app_dir, directory, description)
        .map_err(skill_dispatch_error)?;
    Ok(json!({ "ok": true, "directory": directory }))
}

fn skills_edit(params: &Value) -> Result<Value, DispatchError> {
    let directory = str_param(params, "directory")?;
    let content = str_param(params, "content")?;
    let (home, app_dir) = skills_dirs()?;
    crate::session::skills_model::edit_skill(&home, &app_dir, directory, content)
        .map_err(skill_dispatch_error)?;
    Ok(json!({ "ok": true }))
}

fn skills_delete(params: &Value) -> Result<Value, DispatchError> {
    let directory = str_param(params, "directory")?;
    let (home, app_dir) = skills_dirs()?;
    crate::session::skills_model::delete_skill(&home, &app_dir, directory)
        .map_err(skill_dispatch_error)?;
    Ok(json!({ "ok": true }))
}

fn skills_adopt(params: &Value) -> Result<Value, DispatchError> {
    let source = parse_skill_source(params)?;
    let directory = str_param(params, "directory")?;
    let dest = optional_str_param(params, "dest")?;
    let (home, app_dir) = skills_dirs()?;
    let dest_name =
        crate::session::skills_model::adopt_skill(&home, &app_dir, &source, directory, dest)
            .map_err(skill_dispatch_error)?;
    Ok(json!({ "ok": true, "directory": dest_name }))
}

fn skills_propagate(params: &Value) -> Result<Value, DispatchError> {
    let directory = str_param(params, "directory")?;
    let agent = str_param(params, "agent")?;
    let (home, app_dir) = skills_dirs()?;
    crate::session::skills_model::propagate_skill(&home, &app_dir, directory, agent)
        .map_err(skill_dispatch_error)?;
    Ok(json!({ "ok": true }))
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
            data: None,
        },
        UiError::StaleWorker => DispatchError {
            code: codes::FORBIDDEN,
            message: "worker generation is no longer active".into(),
            data: None,
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

/// `ui.open_url`: a worker asks the surface to open a URL it computed (rather
/// than one already sitting in a `(slot, id)` UI-state entry an `open-ui-link`
/// command reads). Delivered as a notification carrying the `href`: the native
/// TUI opens it directly on first display, and the web renders a click-to-open
/// toast (an async push cannot `window.open` without the popup blocker). The
/// URL must be `http`/`https`; the store rejects anything else.
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

    fn ctx_for(plugin_id: &str, caps: &[&str]) -> PluginRpcContext {
        PluginRpcContext {
            plugin_id: plugin_id.to_string(),
            granted_capabilities: caps.iter().map(|c| c.to_string()).collect(),
            ui_contributions: HashSet::new(),
            ui_generation: 0,
        }
    }

    #[test]
    fn plugin_storage_roundtrip_namespace_and_persistence() {
        let tmp = tempfile::tempdir().unwrap();
        let cron = ctx_for("cron", &[CAP_WORKER]);
        let other = ctx_for("other", &[CAP_WORKER]);
        {
            let state = state(tmp.path());
            // Absent key reads Null.
            let got = dispatch(
                &state,
                &cron,
                "plugin.storage.get",
                &json!({"key": "watermark"}),
            )
            .unwrap();
            assert_eq!(got, json!({ "value": Value::Null }));

            dispatch(
                &state,
                &cron,
                "plugin.storage.set",
                &json!({"key": "watermark", "value": {"seq": 7}}),
            )
            .unwrap();
            let got = dispatch(
                &state,
                &cron,
                "plugin.storage.get",
                &json!({"key": "watermark"}),
            )
            .unwrap();
            assert_eq!(got, json!({ "value": {"seq": 7} }));

            // Another plugin sharing the same key sees its own (absent) value:
            // the namespace is the plugin id, not addressable from the payload.
            let got = dispatch(
                &state,
                &other,
                "plugin.storage.get",
                &json!({"key": "watermark"}),
            )
            .unwrap();
            assert_eq!(got, json!({ "value": Value::Null }));

            let removed = dispatch(
                &state,
                &cron,
                "plugin.storage.remove",
                &json!({"key": "watermark"}),
            )
            .unwrap();
            assert_eq!(removed, json!({ "removed": true }));
            dispatch(
                &state,
                &cron,
                "plugin.storage.set",
                &json!({"key": "watermark", "value": "kept"}),
            )
            .unwrap();
        }
        // Reopen the store (daemon restart): the value survives.
        let state = state(tmp.path());
        let got = dispatch(
            &state,
            &cron,
            "plugin.storage.get",
            &json!({"key": "watermark"}),
        )
        .unwrap();
        assert_eq!(got, json!({ "value": "kept" }));
    }

    #[test]
    fn plugin_storage_cas_swaps_only_on_match() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state(tmp.path());
        let c = ctx(&[CAP_WORKER]);

        // CAS from absent (expected null) creates the key.
        let out = dispatch(
            &state,
            &c,
            "plugin.storage.cas",
            &json!({"key": "k", "expected": null, "value": 1}),
        )
        .unwrap();
        assert_eq!(out, json!({ "swapped": true, "current": 1 }));

        // Wrong expected: no swap, returns the actual current value.
        let out = dispatch(
            &state,
            &c,
            "plugin.storage.cas",
            &json!({"key": "k", "expected": 99, "value": 2}),
        )
        .unwrap();
        assert_eq!(out, json!({ "swapped": false, "current": 1 }));

        // Matching expected swaps.
        let out = dispatch(
            &state,
            &c,
            "plugin.storage.cas",
            &json!({"key": "k", "expected": 1, "value": 2}),
        )
        .unwrap();
        assert_eq!(out, json!({ "swapped": true, "current": 2 }));

        // Omitting `expected` is a malformed request, not a create-if-absent.
        let err = dispatch(
            &state,
            &c,
            "plugin.storage.cas",
            &json!({"key": "k", "value": 3}),
        )
        .unwrap_err();
        assert_eq!(err.code, codes::INVALID_PARAMS);
    }

    #[test]
    fn plugin_storage_enforces_quotas() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state(tmp.path());
        let c = ctx(&[CAP_WORKER]);

        // Value size cap.
        let big = "x".repeat(STORAGE_MAX_VALUE_BYTES + 1);
        let err = dispatch(
            &state,
            &c,
            "plugin.storage.set",
            &json!({"key": "k", "value": big}),
        )
        .unwrap_err();
        assert_eq!(err.code, codes::FORBIDDEN);
        assert_eq!(err.data.as_ref().unwrap()["kind"], "storage_quota_exceeded");

        // Key-count cap: fill to the limit, then a NEW key is refused but an
        // overwrite of an existing key still succeeds.
        for i in 0..STORAGE_MAX_KEYS {
            dispatch(
                &state,
                &c,
                "plugin.storage.set",
                &json!({"key": format!("k{i}"), "value": i}),
            )
            .unwrap();
        }
        let err = dispatch(
            &state,
            &c,
            "plugin.storage.set",
            &json!({"key": "overflow", "value": 1}),
        )
        .unwrap_err();
        assert_eq!(err.data.as_ref().unwrap()["kind"], "storage_quota_exceeded");
        // Overwriting an existing key is always allowed.
        dispatch(
            &state,
            &c,
            "plugin.storage.set",
            &json!({"key": "k0", "value": "updated"}),
        )
        .unwrap();
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
        use crate::session::{update_config, PluginConfig};

        let tmp = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("XDG_CONFIG_HOME");
        std::env::set_var("XDG_CONFIG_HOME", tmp.path());

        // Seed the global config with one setting under "acme.worker".
        update_config(|config| {
            let mut plugin = PluginConfig::default();
            plugin
                .settings
                .insert("poll_interval_ms".to_string(), toml::Value::Integer(5000));
            config.plugins.insert("acme.worker".to_string(), plugin);
        })
        .unwrap();

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
    fn ui_state_set_settings_page_requires_declaration() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state(tmp.path());
        // Declared settings-page/"main"; an undeclared settings-page/"other" is
        // refused by the same generic (slot, id) guard.
        let c = ui_ctx(&state, &[CAP_WORKER], UiSlot::SettingsPage, "main");
        let err = dispatch(
            &state,
            &c,
            "ui.state.set",
            &json!({"slot": "settings-page", "id": "other", "payload": {"title": "x"}}),
        )
        .unwrap_err();
        assert_eq!(err.code, codes::FORBIDDEN);

        // The declared global page succeeds and surfaces in the snapshot.
        dispatch(
            &state,
            &c,
            "ui.state.set",
            &json!({"slot": "settings-page", "id": "main", "payload": {"title": "MCP", "blocks": [{"kind": "heading", "text": "Servers"}]}}),
        )
        .unwrap();
        let snap = state.ui_snapshot();
        assert_eq!(snap.entries.len(), 1);
        assert!(
            snap.entries[0].session_id.is_none(),
            "settings-page is global"
        );
    }

    #[test]
    fn ui_state_set_requires_declared_tool_card_badge_slot() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state(tmp.path());
        // The plugin declared a different slot, so pushing tool-card-badge is
        // refused by require_declared_slot even though the payload is valid.
        let c = ui_ctx(&state, &[CAP_WORKER], UiSlot::DetailBadge, "provenance");
        let payload =
            json!({"items": [{"target": {"kind": "mcp", "name": "github"}, "text": "MCP"}]});
        let err = dispatch(
            &state,
            &c,
            "ui.state.set",
            &json!({"slot": "tool-card-badge", "id": "provenance", "session_id": "s1", "payload": payload}),
        )
        .unwrap_err();
        assert_eq!(err.code, codes::FORBIDDEN);

        // Declaring the slot lets the same push through, and it surfaces in the
        // snapshot.
        let c = ui_ctx(&state, &[CAP_WORKER], UiSlot::ToolCardBadge, "provenance");
        dispatch(
            &state,
            &c,
            "ui.state.set",
            &json!({"slot": "tool-card-badge", "id": "provenance", "session_id": "s1", "payload": payload}),
        )
        .unwrap();
        let snap = state.ui_snapshot();
        assert_eq!(snap.entries.len(), 1);
        assert_eq!(snap.entries[0].slot, UiSlot::ToolCardBadge);
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
    fn ui_open_url_requires_browser_open_and_validates_scheme() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state(tmp.path());
        // runtime.worker alone cannot open a URL.
        let c = ui_ctx(&state, &[CAP_WORKER], UiSlot::Notification, "n");
        let err = dispatch(
            &state,
            &c,
            "ui.open_url",
            &json!({"url": "https://example.com"}),
        )
        .unwrap_err();
        assert_eq!(err.code, codes::FORBIDDEN);

        // With browser_open, a non-http scheme is rejected.
        let c = ui_ctx(&state, &[CAP_BROWSER_OPEN], UiSlot::Notification, "n");
        let err = dispatch(
            &state,
            &c,
            "ui.open_url",
            &json!({"url": "file:///etc/passwd"}),
        )
        .unwrap_err();
        assert_eq!(err.code, codes::INVALID_PARAMS);

        // A valid https URL posts a notification carrying the href.
        let ok = dispatch(
            &state,
            &c,
            "ui.open_url",
            &json!({"url": "https://example.com/pr/1"}),
        )
        .unwrap();
        assert_eq!(ok["seq"], json!(1));
        let notifs = state.ui_snapshot().notifications;
        assert_eq!(notifs.len(), 1);
        assert_eq!(notifs[0].href.as_deref(), Some("https://example.com/pr/1"));
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

    #[test]
    fn composer_action_draft_operation_requires_composer_write() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state(tmp.path());
        let c = ui_ctx(&state, &[CAP_WORKER], UiSlot::ComposerAction, "voice");

        dispatch(
            &state,
            &c,
            "ui.state.set",
            &json!({
                "slot": "composer-action",
                "id": "voice",
                "session_id": "s1",
                "payload": {"label": "Voice", "method": "voice.start"}
            }),
        )
        .unwrap();

        let err = dispatch(
            &state,
            &c,
            "ui.state.set",
            &json!({
                "slot": "composer-action",
                "id": "voice",
                "session_id": "s1",
                "payload": {
                    "label": "Voice",
                    "method": "voice.start",
                    "draft_operation": {"kind": "insert-text", "id": "op-1", "text": "hello"}
                }
            }),
        )
        .unwrap_err();
        assert_eq!(err.code, codes::FORBIDDEN);

        let c = ui_ctx(
            &state,
            &[CAP_WORKER, CAP_COMPOSER_WRITE],
            UiSlot::ComposerAction,
            "voice",
        );
        dispatch(
            &state,
            &c,
            "ui.state.set",
            &json!({
                "slot": "composer-action",
                "id": "voice",
                "session_id": "s1",
                "payload": {
                    "label": "Voice",
                    "method": "voice.start",
                    "draft_operation": {"kind": "insert-text", "id": "op-1", "text": "hello"}
                }
            }),
        )
        .unwrap();
    }

    /// Restore HOME + XDG_CONFIG_HOME on drop so a failing assertion never leaks
    /// the temp override into the rest of the test process.
    struct HomeGuard {
        home: Option<std::ffi::OsString>,
        xdg: Option<std::ffi::OsString>,
    }
    impl Drop for HomeGuard {
        fn drop(&mut self) {
            match self.home.take() {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            match self.xdg.take() {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
        }
    }

    /// Point `$HOME` and `$XDG_CONFIG_HOME` at a temp dir so `get_app_dir` and
    /// skills discovery resolve under it, on both Linux and macOS. Restores both
    /// on drop; serialized by `#[serial]` because it mutates process env.
    struct SkillsEnvGuard {
        _tmp: tempfile::TempDir,
        prev_home: Option<std::ffi::OsString>,
        prev_xdg: Option<std::ffi::OsString>,
    }

    impl SkillsEnvGuard {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let prev_home = std::env::var_os("HOME");
            let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
            std::env::set_var("HOME", tmp.path());
            std::env::set_var("XDG_CONFIG_HOME", tmp.path().join(".config"));
            Self {
                _tmp: tmp,
                prev_home,
                prev_xdg,
            }
        }

        fn home(&self) -> PathBuf {
            self._tmp.path().to_path_buf()
        }
    }

    impl Drop for SkillsEnvGuard {
        fn drop(&mut self) {
            match self.prev_home.take() {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            match self.prev_xdg.take() {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
        }
    }

    fn set_tmp_home(dir: &std::path::Path) -> HomeGuard {
        let guard = HomeGuard {
            home: std::env::var_os("HOME"),
            xdg: std::env::var_os("XDG_CONFIG_HOME"),
        };
        std::env::set_var("HOME", dir);
        std::env::set_var("XDG_CONFIG_HOME", dir.join(".config"));
        guard
    }

    /// Story: a plugin with `config.write` calls `mcp.add`, and `mcp.list`
    /// returns the server on the next call, resolved from the `global` layer.
    #[test]
    #[serial_test::serial]
    fn mcp_add_then_list_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let _home = set_tmp_home(tmp.path());
        let state = state(tmp.path());
        let c = ctx(&[CAP_CONFIG_READ, CAP_CONFIG_WRITE]);

        let added = dispatch(
            &state,
            &c,
            "mcp.add",
            &json!({"name": "fs", "command": "mcp-fs", "args": ["--root", "."]}),
        )
        .unwrap();
        assert_eq!(added["status"], json!("added"));

        // A second add of the same name is refused (use edit).
        let dup = dispatch(
            &state,
            &c,
            "mcp.add",
            &json!({"name": "fs", "command": "x"}),
        )
        .unwrap_err();
        assert_eq!(dup.code, codes::INVALID_PARAMS);

        let list = dispatch(&state, &c, "mcp.list", &json!({"agent": "claude"})).unwrap();
        let servers = list["servers"].as_array().unwrap();
        let fs = servers.iter().find(|s| s["name"] == json!("fs")).unwrap();
        assert_eq!(fs["command"], json!("mcp-fs"));
        assert_eq!(fs["provenance"], json!("global"));
    }

    /// Story: `mcp.delete` removes a `global` server; targeting an `agent-native`
    /// server returns FORBIDDEN and writes nothing; an unknown name is
    /// INVALID_PARAMS.
    #[test]
    #[serial_test::serial]
    fn mcp_delete_global_removes_but_agent_native_is_forbidden() {
        let tmp = tempfile::tempdir().unwrap();
        let _home = set_tmp_home(tmp.path());
        // A native (claude) server that AoE does not own.
        std::fs::write(
            tmp.path().join(".claude.json"),
            r#"{ "mcpServers": { "native": { "command": "n" } } }"#,
        )
        .unwrap();
        let state = state(tmp.path());
        let c = ctx(&[CAP_CONFIG_READ, CAP_CONFIG_WRITE]);

        dispatch(
            &state,
            &c,
            "mcp.add",
            &json!({"name": "g", "command": "gcmd"}),
        )
        .unwrap();
        let deleted = dispatch(&state, &c, "mcp.delete", &json!({"name": "g"})).unwrap();
        assert_eq!(deleted["status"], json!("deleted"));

        // The agent-native server is not AoE-owned: refused, and no global write.
        let forbidden = dispatch(
            &state,
            &c,
            "mcp.delete",
            &json!({"name": "native", "agent": "claude"}),
        )
        .unwrap_err();
        assert_eq!(forbidden.code, codes::FORBIDDEN);
        // The forbidden delete wrote nothing to the global layer.
        assert!(!crate::session::mcp_overrides::remove_global_server("native").unwrap());

        let missing = dispatch(
            &state,
            &c,
            "mcp.delete",
            &json!({"name": "nope", "agent": "claude"}),
        )
        .unwrap_err();
        assert_eq!(missing.code, codes::INVALID_PARAMS);
    }

    /// Story: `mcp.add` / `mcp.edit` refuse a name owned by a non-global layer
    /// with FORBIDDEN (AoE only writes the global layer), and `mcp.edit` on a
    /// name that exists nowhere globally is INVALID_PARAMS.
    #[test]
    #[serial_test::serial]
    fn mcp_add_and_edit_reject_non_global_names() {
        let tmp = tempfile::tempdir().unwrap();
        let _home = set_tmp_home(tmp.path());
        // An agent-native server AoE does not own.
        std::fs::write(
            tmp.path().join(".claude.json"),
            r#"{ "mcpServers": { "native": { "command": "n" } } }"#,
        )
        .unwrap();
        let state = state(tmp.path());
        let c = ctx(&[CAP_CONFIG_READ, CAP_CONFIG_WRITE]);

        // add of a native-owned name: FORBIDDEN, and no global override written.
        let add_forbidden = dispatch(
            &state,
            &c,
            "mcp.add",
            &json!({"name": "native", "command": "x", "agent": "claude"}),
        )
        .unwrap_err();
        assert_eq!(add_forbidden.code, codes::FORBIDDEN);
        assert!(!crate::session::mcp_overrides::remove_global_server("native").unwrap());

        // edit of a native-owned name: FORBIDDEN (not INVALID_PARAMS).
        let edit_forbidden = dispatch(
            &state,
            &c,
            "mcp.edit",
            &json!({"name": "native", "command": "x", "agent": "claude"}),
        )
        .unwrap_err();
        assert_eq!(edit_forbidden.code, codes::FORBIDDEN);

        // edit of a name that exists nowhere globally: INVALID_PARAMS (use add).
        let edit_missing = dispatch(
            &state,
            &c,
            "mcp.edit",
            &json!({"name": "ghost", "command": "x", "agent": "claude"}),
        )
        .unwrap_err();
        assert_eq!(edit_missing.code, codes::INVALID_PARAMS);
    }

    /// Story: a plugin without `config.write` cannot perform any MCP write or a
    /// `config.write`; the host refuses on capability before the handler runs.
    #[test]
    fn mcp_and_config_writes_require_config_write_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state(tmp.path());
        // Holds config.read but not config.write.
        let c = ctx(&[CAP_CONFIG_READ]);

        for (method, params) in [
            ("mcp.add", json!({"name": "fs", "command": "c"})),
            ("mcp.edit", json!({"name": "fs", "command": "c"})),
            ("mcp.delete", json!({"name": "fs"})),
            ("mcp.keep", json!({"name": "fs"})),
            ("mcp.drop", json!({"name": "fs"})),
            (
                "mcp.resolve-conflict",
                json!({"name": "fs", "winner": "aoe", "fingerprint": "x"}),
            ),
            (
                "config.write",
                json!({"patch": {"session": {"yolo_mode_default": true}}}),
            ),
        ] {
            let err = dispatch(&state, &c, method, &params).unwrap_err();
            assert_eq!(
                err.code,
                codes::FORBIDDEN,
                "{method} must require config.write"
            );
        }
    }

    /// Story: `config.write` refuses an unknown section, a host-execution
    /// (`local_only`) field, and an elevation-required field, then accepts a
    /// plain field which round-trips through `config.read`.
    #[test]
    #[serial_test::serial]
    fn config_write_gates_fields_and_round_trips_allowed() {
        let tmp = tempfile::tempdir().unwrap();
        let _home = set_tmp_home(tmp.path());
        let state = state(tmp.path());
        let c = ctx(&[CAP_CONFIG_READ, CAP_CONFIG_WRITE]);

        // Unknown section (`hooks` = arbitrary shell, no descriptor) -> rejected.
        let unknown = dispatch(
            &state,
            &c,
            "config.write",
            &json!({"patch": {"hooks": {"on_start": "rm -rf /"}}}),
        )
        .unwrap_err();
        assert_eq!(unknown.code, codes::INVALID_PARAMS);

        // local_only host-execution surface -> FORBIDDEN.
        let local_only = dispatch(
            &state,
            &c,
            "config.write",
            &json!({"patch": {"acp": {"node_path": "/tmp/node"}}}),
        )
        .unwrap_err();
        assert_eq!(local_only.code, codes::FORBIDDEN);

        // Elevation-required field -> FORBIDDEN (a plugin gets the unelevated set).
        let elevated = dispatch(
            &state,
            &c,
            "config.write",
            &json!({"patch": {"worktree": {"enabled": true}}}),
        )
        .unwrap_err();
        assert_eq!(elevated.code, codes::FORBIDDEN);

        // A plain Allow field writes and reads back.
        dispatch(
            &state,
            &c,
            "config.write",
            &json!({"patch": {"session": {"yolo_mode_default": true}}}),
        )
        .unwrap();
        let read = dispatch(
            &state,
            &c,
            "config.read",
            &json!({"section": "session", "field": "yolo_mode_default"}),
        )
        .unwrap();
        assert_eq!(read["value"], json!(true));

        // An unknown field is rejected on read too.
        let bad = dispatch(
            &state,
            &c,
            "config.read",
            &json!({"section": "session", "field": "nope"}),
        )
        .unwrap_err();
        assert_eq!(bad.code, codes::INVALID_PARAMS);

        // config.read is gated symmetrically with config.write: a host-execution
        // (`local_only`) field and an elevation-gated field, which can carry
        // literal secrets (e.g. `sandbox.environment`), are FORBIDDEN to read,
        // not just to write.
        for (section, field) in [
            ("acp", "node_path"),       // local_only host-execution surface
            ("worktree", "enabled"),    // elevation-gated
            ("sandbox", "environment"), // elevation-gated, may hold secrets
        ] {
            let err = dispatch(
                &state,
                &c,
                "config.read",
                &json!({"section": section, "field": field}),
            )
            .unwrap_err();
            assert_eq!(
                err.code,
                codes::FORBIDDEN,
                "config.read of {section}.{field} must be FORBIDDEN"
            );
        }
    }

    fn write_host_skill(home: &Path, agent_rel: &str, directory: &str) {
        let d = home.join(agent_rel).join(directory);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("SKILL.md"),
            format!("---\nname: {directory}\ndescription: host skill\n---\n\nbody\n"),
        )
        .unwrap();
    }

    /// User story 1: with `fs.write`, `skills.create` writes a scaffolded
    /// SKILL.md to the managed store and `skills.list` returns it as
    /// `aoe-managed` and writable.
    #[test]
    #[serial_test::serial]
    fn skills_create_then_list_is_managed() {
        let _env = SkillsEnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        let state = state(tmp.path());
        let c = ctx(&[CAP_FS_WRITE, CAP_FS_READ]);

        dispatch(
            &state,
            &c,
            "skills.create",
            &json!({"directory": "my-skill", "description": "use for X"}),
        )
        .unwrap();

        let list = dispatch(&state, &c, "skills.list", &json!({})).unwrap();
        let entry = list["skills"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["directory"] == json!("my-skill"))
            .expect("created skill appears in list");
        assert_eq!(entry["provenance_label"], json!("aoe-managed"));
        assert_eq!(entry["writable"], json!(true));

        let read = dispatch(
            &state,
            &c,
            "skills.read",
            &json!({"source": {"kind": "aoe-managed"}, "directory": "my-skill"}),
        )
        .unwrap();
        assert_eq!(read["name"], json!("my-skill"));
        assert!(read["content"].as_str().unwrap().contains("use for X"));
    }

    /// User story 2: `skills.adopt` copies a host skill into the managed store,
    /// leaves the host original untouched, and editing a still-host-only skill
    /// is FORBIDDEN.
    #[test]
    #[serial_test::serial]
    fn skills_adopt_leaves_host_and_host_edit_is_forbidden() {
        let env = SkillsEnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        let state = state(tmp.path());
        let c = ctx(&[CAP_FS_WRITE, CAP_FS_READ]);

        write_host_skill(&env.home(), ".claude/skills", "review");
        write_host_skill(&env.home(), ".claude/skills", "hostonly");
        let src = env.home().join(".claude/skills/review/SKILL.md");
        let before = std::fs::read_to_string(&src).unwrap();

        dispatch(
            &state,
            &c,
            "skills.adopt",
            &json!({"source": {"kind": "agent-native", "agent": "claude"}, "directory": "review"}),
        )
        .unwrap();
        // Host original untouched; managed copy now readable.
        assert_eq!(std::fs::read_to_string(&src).unwrap(), before);
        dispatch(
            &state,
            &c,
            "skills.read",
            &json!({"source": {"kind": "aoe-managed"}, "directory": "review"}),
        )
        .unwrap();

        // Editing a skill that exists only host-side is refused.
        let err = dispatch(
            &state,
            &c,
            "skills.edit",
            &json!({"directory": "hostonly", "content": "---\nname: hostonly\ndescription: d\n---\n\nx\n"}),
        )
        .unwrap_err();
        assert_eq!(err.code, codes::FORBIDDEN);
    }

    /// User story 3: a plugin without `fs.write` is refused on every skills
    /// write and on `fs.write`.
    #[test]
    #[serial_test::serial]
    fn skills_writes_require_fs_write() {
        let _env = SkillsEnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        let state = state(tmp.path());
        let c = ctx(&[CAP_FS_READ]);

        for (method, params) in [
            ("skills.create", json!({"directory": "x"})),
            (
                "skills.edit",
                json!({"directory": "x", "content": "---\nname: x\ndescription: d\n---\n"}),
            ),
            ("skills.delete", json!({"directory": "x"})),
            (
                "skills.adopt",
                json!({"source": {"kind": "agent-native", "agent": "claude"}, "directory": "x"}),
            ),
            (
                "skills.propagate",
                json!({"directory": "x", "agent": "kimi"}),
            ),
            (
                "fs.write",
                json!({"root": "plugin", "path": "a.txt", "content": "hi"}),
            ),
        ] {
            let err = dispatch(&state, &c, method, &params).unwrap_err();
            assert_eq!(err.code, codes::FORBIDDEN, "{method} should be forbidden");
        }
    }

    /// `fs.*` round-trips within a root and rejects traversal / absolute paths.
    #[test]
    #[serial_test::serial]
    fn fs_read_write_roundtrip_and_rejects_escape() {
        let _env = SkillsEnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        let state = state(tmp.path());
        let c = ctx(&[CAP_FS_READ, CAP_FS_WRITE]);

        dispatch(
            &state,
            &c,
            "fs.write",
            &json!({"root": "plugin", "path": "notes/a.txt", "content": "hello"}),
        )
        .unwrap();
        let got = dispatch(
            &state,
            &c,
            "fs.read",
            &json!({"root": "plugin", "path": "notes/a.txt"}),
        )
        .unwrap();
        assert_eq!(got["content"], json!("hello"));

        for bad in ["../escape.txt", "/etc/passwd", "a/../../b"] {
            let err = dispatch(
                &state,
                &c,
                "fs.write",
                &json!({"root": "plugin", "path": bad, "content": "x"}),
            )
            .unwrap_err();
            assert_eq!(err.code, codes::INVALID_PARAMS, "{bad} should be rejected");
        }
        // Unknown root is caller error.
        let err = dispatch(
            &state,
            &c,
            "fs.read",
            &json!({"root": "bogus", "path": "a"}),
        )
        .unwrap_err();
        assert_eq!(err.code, codes::INVALID_PARAMS);
    }

    /// `skills.list` accepts `config.read` as an alternative to `fs.read`, and is
    /// refused with neither.
    #[test]
    #[serial_test::serial]
    fn skills_list_accepts_fs_read_or_config_read() {
        let _env = SkillsEnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        let state = state(tmp.path());

        dispatch(&state, &ctx(&[CAP_CONFIG_READ]), "skills.list", &json!({})).unwrap();
        dispatch(&state, &ctx(&[CAP_FS_READ]), "skills.list", &json!({})).unwrap();
        let err = dispatch(&state, &ctx(&[CAP_WORKER]), "skills.list", &json!({})).unwrap_err();
        assert_eq!(err.code, codes::FORBIDDEN);
    }

    /// `fs.write` must not create any directory outside the root when a path
    /// component is a symlink pointing out: containment is checked before each
    /// mkdir, so the escape is refused with no filesystem side effect.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn fs_write_refuses_to_create_through_symlink() {
        use std::os::unix::fs::symlink;
        let _env = SkillsEnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        let state = state(tmp.path());
        let c = ctx(&[CAP_FS_WRITE]);

        let app_dir = crate::session::get_app_dir().unwrap();
        let files = app_dir.join("plugins").join("acme.worker").join("files");
        std::fs::create_dir_all(&files).unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        symlink(&outside, files.join("link")).unwrap();

        let err = dispatch(
            &state,
            &c,
            "fs.write",
            &json!({"root": "plugin", "path": "link/new/file.txt", "content": "x"}),
        )
        .unwrap_err();
        assert_eq!(err.code, codes::FORBIDDEN);
        // No directory was materialized through the symlink.
        assert!(!outside.join("new").exists());
    }
}
