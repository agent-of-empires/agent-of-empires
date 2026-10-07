//! The bounded per-tool-call context cache that fills in permission requests
//! whose raw input arrived empty.

use crate::acp::state::Event;
use agent_client_protocol::schema::v1::SessionUpdate;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

pub(super) type ToolContextCache = Arc<std::sync::Mutex<ToolCallContextCache>>;

pub(super) const TOOL_CONTEXT_CACHE_LIMIT: usize = 256;

#[derive(Debug, Default)]
pub(super) struct ToolCallContextCache {
    raw_inputs: HashMap<String, serde_json::Value>,
    insertion_order: VecDeque<String>,
}

impl ToolCallContextCache {
    fn record(&mut self, tool_call_id: String, raw_input: serde_json::Value) {
        if raw_input_has_no_user_context(&raw_input) {
            return;
        }
        if !self.raw_inputs.contains_key(&tool_call_id) {
            self.insertion_order.push_back(tool_call_id.clone());
        }
        self.raw_inputs.insert(tool_call_id, raw_input);
        self.enforce_limit();
    }

    pub(super) fn get(&self, tool_call_id: &str) -> Option<serde_json::Value> {
        self.raw_inputs.get(tool_call_id).cloned()
    }

    fn remove(&mut self, tool_call_id: &str) {
        self.raw_inputs.remove(tool_call_id);
        self.insertion_order.retain(|id| id != tool_call_id);
    }

    fn enforce_limit(&mut self) {
        while self.raw_inputs.len() > TOOL_CONTEXT_CACHE_LIMIT {
            let Some(oldest) = self.insertion_order.pop_front() else {
                break;
            };
            self.raw_inputs.remove(&oldest);
        }
    }
}

pub(super) fn permission_raw_input_with_context(
    permission_raw: Option<&serde_json::Value>,
    cached_raw: Option<&serde_json::Value>,
) -> Option<serde_json::Value> {
    let cached_raw =
        cached_raw.and_then(|cached| (!raw_input_has_no_user_context(cached)).then_some(cached));
    match (permission_raw, cached_raw) {
        (Some(permission), Some(cached)) => Some(merge_permission_raw_input(permission, cached)),
        (Some(permission), None) if permission.is_null() => None,
        (Some(permission), None) => Some(permission.clone()),
        (None, Some(cached)) => Some(cached.clone()),
        (None, None) => None,
    }
}

pub(super) fn merge_permission_raw_input(
    permission_raw: &serde_json::Value,
    cached_raw: &serde_json::Value,
) -> serde_json::Value {
    if let (serde_json::Value::Object(permission), serde_json::Value::Object(cached)) =
        (permission_raw, cached_raw)
    {
        let mut merged = cached.clone();
        for (key, value) in permission {
            merged.insert(key.clone(), value.clone());
        }
        return serde_json::Value::Object(merged);
    }

    if raw_input_has_no_user_context(permission_raw) {
        cached_raw.clone()
    } else {
        permission_raw.clone()
    }
}

pub(super) fn raw_input_has_no_user_context(raw_input: &serde_json::Value) -> bool {
    match raw_input {
        serde_json::Value::Null => true,
        serde_json::Value::Object(map) => map.keys().all(|key| key.starts_with("_aoe_")),
        _ => false,
    }
}

pub(super) fn update_tool_context_cache(
    cache: &ToolContextCache,
    event: &Event,
    source_update: &SessionUpdate,
) {
    match event {
        Event::ToolCallStarted { tool_call } => {
            if let Some(raw_input) = raw_input_for_tool_event(source_update, &tool_call.id) {
                cache
                    .lock()
                    .expect("tool context cache mutex poisoned")
                    .record(tool_call.id.clone(), raw_input);
            }
        }
        Event::ToolCallUpdated { tool_call_id, .. } => {
            if let Some(raw_input) = raw_input_for_tool_event(source_update, tool_call_id) {
                cache
                    .lock()
                    .expect("tool context cache mutex poisoned")
                    .record(tool_call_id.clone(), raw_input);
            }
        }
        Event::ToolCallCompleted { tool_call_id, .. } => {
            cache
                .lock()
                .expect("tool context cache mutex poisoned")
                .remove(tool_call_id);
        }
        _ => {}
    }
}

pub(super) fn raw_input_for_tool_event(
    source_update: &SessionUpdate,
    tool_call_id: &str,
) -> Option<serde_json::Value> {
    match source_update {
        SessionUpdate::ToolCall(tool_call)
            if tool_call.tool_call_id.0.to_string() == tool_call_id =>
        {
            tool_call.raw_input.clone()
        }
        SessionUpdate::ToolCallUpdate(update)
            if update.tool_call_id.0.to_string() == tool_call_id =>
        {
            update.fields.raw_input.clone()
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tool_context_cases() {
        let cached = json!({ "command": "mkdir -p /tmp/opencode", "workdir": "/tmp" });
        // (request, cached, expected)
        let cases = [
            // An empty request falls back to the cached context.
            (None, Some(cached.clone()), Some(cached.clone())),
            // The request wins shared keys; cached fills the rest.
            (
                Some(json!({ "filepath": "/tmp/opencode", "command": "permission command" })),
                Some(cached.clone()),
                Some(json!({
                    "filepath": "/tmp/opencode",
                    "command": "permission command",
                    "workdir": "/tmp",
                })),
            ),
            // Bookkeeping-only requests keep their aoe metadata.
            (
                Some(json!({ "_aoe_title": "external_directory" })),
                Some(cached.clone()),
                Some(json!({
                    "_aoe_title": "external_directory",
                    "command": "mkdir -p /tmp/opencode",
                    "workdir": "/tmp",
                })),
            ),
            // A non-empty non-object request is left as is.
            (
                Some(json!(["already", "specific"])),
                Some(cached.clone()),
                Some(json!(["already", "specific"])),
            ),
            (None, Some(json!({})), None),
        ];
        for (request, cached, expected) in cases {
            assert_eq!(
                permission_raw_input_with_context(request.as_ref(), cached.as_ref()),
                expected,
                "{request:?}"
            );
        }

        {
            let mut cache = ToolCallContextCache::default();
            cache.record("empty".to_string(), json!({}));
            assert!(cache.get("empty").is_none());

            cache.record("tc-1".to_string(), json!({ "command": "ls" }));
            assert!(cache.get("tc-1").is_some());
            cache.remove("tc-1");
            assert!(cache.get("tc-1").is_none());
            assert!(!cache.insertion_order.iter().any(|id| id == "tc-1"));

            for idx in 0..=TOOL_CONTEXT_CACHE_LIMIT {
                cache.record(format!("tc-{idx}"), json!({ "idx": idx }));
            }
            assert_eq!(cache.raw_inputs.len(), TOOL_CONTEXT_CACHE_LIMIT);
            assert!(cache.get("tc-0").is_none());
            assert!(cache
                .get(&format!("tc-{TOOL_CONTEXT_CACHE_LIMIT}"))
                .is_some());
        }
    }
}
