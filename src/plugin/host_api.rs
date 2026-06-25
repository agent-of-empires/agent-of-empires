//! The capability-gated host API a plugin worker calls over the worker
//! protocol.
//!
//! Every method maps to a capability the plugin must have declared in its
//! manifest and had granted at install. The middleware
//! ([`PluginRpcContext::require`]) refuses an undeclared or ungranted call
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
//!
//! Per-plugin namespace: session metadata is always read and written under the
//! calling plugin's own `Instance.plugin_meta[<plugin-id>]` slot. The worker
//! sends only `key`; it can never name another plugin's id, so one plugin
//! cannot touch another's metadata.

use std::sync::Mutex;

use rusqlite::Connection;
use serde_json::{json, Value};

use crate::events::{self, Order, Schema, SeqBound};
use crate::plugin::protocol::codes;
use crate::session::Storage;

/// Capability required by each host method. Reused from the manifest taxonomy
/// (`aoe_plugin_api::KNOWN_CAPABILITIES`); no new capability is introduced.
const CAP_WORKER: &str = "runtime.worker";
const CAP_SESSION_READ: &str = "session.read";
const CAP_SESSION_WRITE: &str = "session.write";

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
        })
    }

    fn storage(&self) -> anyhow::Result<Storage> {
        Storage::new_unwatched(&self.profile)
    }
}

/// Per-worker call context: who is calling and what they were granted. Built
/// once when the worker connects, from its `LoadedPlugin`.
pub struct PluginRpcContext {
    pub plugin_id: String,
    pub granted_capabilities: Vec<String>,
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
            sessions_list(state)
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

fn sessions_list(state: &HostApiState) -> Result<Value, DispatchError> {
    let storage = state
        .storage()
        .map_err(|e| DispatchError::internal(e.to_string()))?;
    let instances = storage
        .load()
        .map_err(|e| DispatchError::internal(e.to_string()))?;
    let sessions: Vec<Value> = instances
        .iter()
        .map(|i| {
            json!({
                "id": i.id,
                "title": i.title,
                "project_path": i.project_path,
                "tool": i.tool,
                "status": format!("{:?}", i.status),
            })
        })
        .collect();
    Ok(json!({ "sessions": sessions }))
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
        };
        let other = dispatch(
            &state,
            &b,
            "session.meta.get",
            &json!({"session_id": session_id, "key": "k"}),
        )
        .unwrap();
        assert_eq!(other["value"], json!(null));

        // sessions.list surfaces the seeded session.
        let list = dispatch(&state, &a, "sessions.list", &json!({})).unwrap();
        let sessions = list["sessions"].as_array().unwrap();
        assert!(sessions.iter().any(|s| s["id"] == json!(session_id)));
    }
}
