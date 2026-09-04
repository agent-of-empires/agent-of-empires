//! The bounded per-tool-call context cache that fills in permission requests
//! whose raw input arrived empty.

use crate::acp::state::Event;
use agent_client_protocol::schema::v1::SessionUpdate;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

/// Per-tool metadata stored in the silent-orphan watchdog's
/// `tool_calls_in_flight` map. Lets the watchdog remember the original
/// `run_in_background` flag observed at `ToolStarted` time so the
/// completion path can flip `off_protocol_work_seen` even if the
/// completion content marker is missing or reshaped. See #1401.
#[derive(Debug, Clone, Copy)]
pub(super) struct ToolMetadata {
    pub(super) is_background_task: bool,
}

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

    #[test]
    fn permission_raw_input_uses_cached_context_when_request_is_empty() {
        let cached = serde_json::json!({ "command": "mkdir -p /tmp/opencode", "workdir": "/tmp" });
        let enriched = permission_raw_input_with_context(None, Some(&cached))
            .expect("cached context should be used");

        assert_eq!(enriched.get("command"), cached.get("command"));
        assert_eq!(enriched.get("workdir"), cached.get("workdir"));
    }

    #[test]
    fn permission_raw_input_merges_cached_context_without_overwriting_request() {
        let permission =
            serde_json::json!({ "filepath": "/tmp/opencode", "command": "permission command" });
        let cached = serde_json::json!({ "command": "mkdir -p /tmp/opencode", "workdir": "/tmp" });
        let enriched = permission_raw_input_with_context(Some(&permission), Some(&cached))
            .expect("non-empty permission args should remain present");

        assert_eq!(enriched.get("command"), permission.get("command"));
        assert_eq!(enriched.get("filepath"), permission.get("filepath"));
        assert_eq!(enriched.get("workdir"), cached.get("workdir"));
    }

    #[test]
    fn permission_raw_input_preserves_aoe_metadata_when_falling_back() {
        let permission = serde_json::json!({ "_aoe_title": "external_directory" });
        let cached = serde_json::json!({ "command": "mkdir -p /tmp/opencode" });
        let enriched = permission_raw_input_with_context(Some(&permission), Some(&cached))
            .expect("cached context should enrich bookkeeping-only requests");

        assert_eq!(enriched.get("_aoe_title"), permission.get("_aoe_title"));
        assert_eq!(enriched.get("command"), cached.get("command"));
    }

    #[test]
    fn permission_raw_input_keeps_non_empty_non_object_request() {
        let permission = serde_json::json!(["already", "specific"]);
        let cached = serde_json::json!({ "command": "mkdir -p /tmp/opencode" });
        let enriched = permission_raw_input_with_context(Some(&permission), Some(&cached))
            .expect("non-empty permission args should remain present");

        assert_eq!(enriched, permission);
    }

    #[test]
    fn permission_raw_input_ignores_empty_cached_context() {
        let cached = serde_json::json!({});
        let enriched = permission_raw_input_with_context(None, Some(&cached));

        assert!(enriched.is_none());
    }

    #[test]
    fn tool_context_cache_ignores_empty_context_entries() {
        let mut cache = ToolCallContextCache::default();
        cache.record("tc-1".to_string(), serde_json::json!({}));

        assert!(cache.get("tc-1").is_none());
    }

    #[test]
    fn tool_context_cache_removes_completed_entries() {
        let mut cache = ToolCallContextCache::default();
        cache.record("tc-1".to_string(), serde_json::json!({ "command": "ls" }));
        assert!(cache.get("tc-1").is_some());

        cache.remove("tc-1");

        assert!(cache.get("tc-1").is_none());
        assert!(!cache.insertion_order.iter().any(|id| id == "tc-1"));
    }

    #[test]
    fn tool_context_cache_enforces_bounded_size() {
        let mut cache = ToolCallContextCache::default();
        for idx in 0..=TOOL_CONTEXT_CACHE_LIMIT {
            cache.record(format!("tc-{idx}"), serde_json::json!({ "idx": idx }));
        }

        assert_eq!(cache.raw_inputs.len(), TOOL_CONTEXT_CACHE_LIMIT);
        assert!(cache.get("tc-0").is_none());
        assert!(cache
            .get(&format!("tc-{TOOL_CONTEXT_CACHE_LIMIT}"))
            .is_some());
    }
}
