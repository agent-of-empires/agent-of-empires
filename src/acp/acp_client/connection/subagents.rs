//! Subagent sessions: the ACP subagents RFD's child sessions, which share the
//! parent's connection and arrive addressed to their own session id.

use agent_client_protocol::schema::MaybeUndefined;
use agent_client_protocol::v1::schema::{SessionId, SessionUpdate, StateUpdate, SubagentUpdate};

use super::notifications::Shared;
use crate::acp::acp_client::config_options::map_acp_config_option;
use crate::acp::acp_client::tool_context::update_tool_context_cache;
use crate::acp::acp_client::update_events::map_update_to_events;
use crate::acp::state::{ConfigOptionCategory, Event, SubagentRecord, SubagentState};

fn patch<T>(slot: &mut Option<T>, incoming: MaybeUndefined<T>) {
    match incoming {
        MaybeUndefined::Undefined => {}
        MaybeUndefined::Null => *slot = None,
        MaybeUndefined::Value(value) => *slot = Some(value),
    }
}

fn apply_state(record: &mut SubagentRecord, state: MaybeUndefined<StateUpdate>) {
    let (state, stop_reason) = match state {
        MaybeUndefined::Undefined => return,
        MaybeUndefined::Null => (SubagentState::Unknown, None),
        MaybeUndefined::Value(StateUpdate::Running(_)) => (SubagentState::Running, None),
        MaybeUndefined::Value(StateUpdate::RequiresAction(_)) => {
            (SubagentState::RequiresAction, None)
        }
        MaybeUndefined::Value(StateUpdate::Idle(idle)) => {
            // The reason is an open string: the RFD reserves future values.
            let reason = idle
                .stop_reason
                .and_then(|reason| serde_json::to_value(reason).ok())
                .and_then(|value| value.get("stopReason")?.as_str().map(str::to_owned));
            (SubagentState::Idle, reason)
        }
        MaybeUndefined::Value(_) => (SubagentState::Unknown, None),
    };
    record.state = state;
    record.stop_reason = stop_reason;
}

impl Shared {
    /// Merge `edit` into the child's snapshot and publish the result.
    async fn publish_subagent(
        &self,
        parent: &SessionId,
        child: &SessionId,
        edit: impl FnOnce(&mut SubagentRecord),
    ) {
        let record = {
            let mut subagents = self.subagents.lock().expect("subagents mutex poisoned");
            let record = subagents
                .entry(child.0.to_string())
                .or_insert_with(|| SubagentRecord {
                    session_id: child.0.to_string(),
                    parent_session_id: parent.0.to_string(),
                    parent_tool_call_id: None,
                    title: None,
                    description: None,
                    state: SubagentState::default(),
                    stop_reason: None,
                    cancel_capable: false,
                    model: None,
                });
            record.cancel_capable = self
                .ingress
                .child_route(child)
                .is_some_and(|route| route.cancel_capable);
            edit(record);
            record.clone()
        };
        self.emit(Event::SubagentUpdated {
            subagent: record,
            at: chrono::Utc::now(),
        })
        .await;
    }

    /// `parent` is the session the announcement arrived on.
    pub(super) async fn handle_subagent_update(&self, parent: SessionId, update: SubagentUpdate) {
        let child = update.session_id.clone();
        // Ingress registers only announcements that name a valid child of the
        // session they arrived on; anything else is dropped here.
        if self
            .ingress
            .child_route(&child)
            .is_none_or(|route| route.parent != parent)
        {
            return;
        }
        let link = match &update.meta {
            MaybeUndefined::Value(meta) => self
                .profile
                .parent_tool_use_id_from_meta(&Some(meta.clone())),
            _ => None,
        };
        self.publish_subagent(&parent, &child, |record| {
            record.parent_session_id = parent.0.to_string();
            if link.is_some() {
                record.parent_tool_call_id = link;
            }
            patch(&mut record.title, update.title);
            patch(&mut record.description, update.description);
            apply_state(record, update.state);
        })
        .await;
    }

    /// Only a child's tool calls and model reach the transcript; its own
    /// messages and lifecycle must never read as the root session's.
    pub(super) async fn handle_child_notification(&self, child: SessionId, update: SessionUpdate) {
        match update {
            SessionUpdate::SubagentUpdate(nested) => {
                self.handle_subagent_update(child, nested).await
            }
            tool @ (SessionUpdate::ToolCall(_) | SessionUpdate::ToolCallUpdate(_)) => {
                let link = self
                    .subagents
                    .lock()
                    .expect("subagents mutex poisoned")
                    .get(child.0.as_ref())
                    .and_then(|record| record.parent_tool_call_id.clone());
                let source = tool.clone();
                for mut event in map_update_to_events(tool, self.profile) {
                    match &mut event {
                        Event::ToolCallStarted { tool_call } => {
                            if tool_call.parent_tool_call_id.is_none() {
                                tool_call.parent_tool_call_id = link.clone();
                            }
                        }
                        Event::ToolCallCompleted { .. }
                        | Event::ToolCallUpdated { .. }
                        | Event::ToolCallContent { .. }
                        | Event::DiffEmitted { .. } => {}
                        _ => continue,
                    }
                    update_tool_context_cache(&self.tool_context_cache, &event, &source);
                    if self.event_tx.send(event).await.is_err() {
                        break;
                    }
                }
            }
            SessionUpdate::ConfigOptionUpdate(options) => {
                let model = options
                    .config_options
                    .into_iter()
                    .filter_map(map_acp_config_option)
                    .find(|option| option.category == ConfigOptionCategory::Model)
                    .map(|option| {
                        option
                            .options
                            .iter()
                            .find(|choice| choice.value == option.current_value)
                            .map_or(option.current_value.clone(), |choice| choice.name.clone())
                    });
                let parent = self
                    .ingress
                    .child_route(&child)
                    .map(|route| route.parent)
                    .unwrap_or_else(|| child.clone());
                self.publish_subagent(&parent, &child, |record| record.model = model)
                    .await;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::acp_client::lifecycle::TerminalClaim;
    use crate::acp::acp_client::session_identity::SessionIngress;
    use crate::acp::agent_profiles::CLAUDE;
    use agent_client_protocol::v1::schema::SessionNotification;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    fn notification(value: serde_json::Value) -> SessionNotification {
        serde_json::from_value(value).expect("notification")
    }

    fn announce(parent: &str, state: &str) -> SessionNotification {
        notification(serde_json::json!({
            "sessionId": parent,
            "update": {
                "sessionUpdate": "subagent_update",
                "sessionId": "child",
                "title": "Explore",
                "state": { "state": state },
                "capabilities": { "cancel": {} },
                "_meta": { "claudeCode": { "parentToolUseId": "tool-1" } },
            },
        }))
    }

    async fn feed(ingress: &SessionIngress, shared: &Shared, value: serde_json::Value) {
        let admitted = ingress
            .notification(notification(value), None)
            .await
            .unwrap();
        if let Some((notification, _guard)) = admitted {
            shared.handle_notification(notification, true).await;
        }
    }

    /// An announcement without the cancel capability publishes a child that
    /// cannot be stopped; a later one that carries it flips the snapshot and
    /// the route together.
    #[tokio::test]
    async fn cancel_capability_flips_when_a_later_update_carries_it() {
        let ingress = Arc::new(SessionIngress::new(Some("root".to_string().into())));
        let (event_tx, mut events) = mpsc::channel(16);
        let (lifecycle_tx, _lifecycle) = mpsc::channel(16);
        let shared = Shared::new(
            event_tx,
            "test".into(),
            &CLAUDE,
            lifecycle_tx,
            Arc::new(TerminalClaim::new()),
            Arc::new(AtomicBool::new(false)),
            None,
            ingress.clone(),
            None,
        );
        let update = |capabilities: serde_json::Value| {
            serde_json::json!({
                "sessionId": "root",
                "update": {
                    "sessionUpdate": "subagent_update",
                    "sessionId": "child",
                    "state": { "state": "running" },
                    "capabilities": capabilities,
                },
            })
        };
        feed(&ingress, &shared, update(serde_json::json!({}))).await;
        assert!(
            !ingress
                .child_route(&"child".to_string().into())
                .unwrap()
                .cancel_capable
        );
        feed(
            &ingress,
            &shared,
            update(serde_json::json!({ "cancel": {} })),
        )
        .await;
        assert!(
            ingress
                .child_route(&"child".to_string().into())
                .unwrap()
                .cancel_capable
        );
        drop(shared);

        let mut flags = Vec::new();
        while let Some(Event::SubagentUpdated { subagent, .. }) = events.recv().await {
            flags.push(subagent.cancel_capable);
        }
        assert_eq!(flags, [false, true]);
    }

    /// A child's tool calls nest under the Agent card that spawned it, while
    /// its messages and lifecycle never reach the root session's transcript.
    #[tokio::test]
    async fn child_traffic_nests_tool_calls_and_stays_out_of_the_root_turn() {
        let ingress = Arc::new(SessionIngress::new(Some("root".to_string().into())));
        let (event_tx, mut events) = mpsc::channel(16);
        let (lifecycle_tx, mut lifecycle) = mpsc::channel(16);
        let shared = Shared::new(
            event_tx,
            "test".into(),
            &CLAUDE,
            lifecycle_tx,
            Arc::new(TerminalClaim::new()),
            Arc::new(AtomicBool::new(false)),
            None,
            ingress.clone(),
            None,
        );
        feed(
            &ingress,
            &shared,
            serde_json::to_value(announce("root", "running")).unwrap(),
        )
        .await;
        feed(
            &ingress,
            &shared,
            serde_json::json!({
                "sessionId": "child",
                "update": { "sessionUpdate": "tool_call", "toolCallId": "child-tool",
                            "title": "Read", "kind": "read", "status": "pending" },
            }),
        )
        .await;
        feed(
            &ingress,
            &shared,
            serde_json::json!({
                "sessionId": "child",
                "update": { "sessionUpdate": "agent_message_chunk",
                            "content": { "type": "text", "text": "child prose" } },
            }),
        )
        .await;
        feed(
            &ingress,
            &shared,
            serde_json::json!({
                "sessionId": "stranger",
                "update": { "sessionUpdate": "tool_call", "toolCallId": "x",
                            "title": "Read", "kind": "read", "status": "pending" },
            }),
        )
        .await;
        feed(
            &ingress,
            &shared,
            serde_json::to_value(announce("child", "idle")).unwrap(),
        )
        .await;
        drop(shared);

        let Some(Event::SubagentUpdated { subagent, .. }) = events.recv().await else {
            panic!("expected the announcement first");
        };
        assert_eq!(subagent.session_id, "child");
        assert_eq!(subagent.parent_session_id, "root");
        assert_eq!(subagent.parent_tool_call_id.as_deref(), Some("tool-1"));
        assert_eq!(subagent.title.as_deref(), Some("Explore"));
        assert_eq!(subagent.state, SubagentState::Running);
        assert!(subagent.cancel_capable);

        let Some(Event::ToolCallStarted { tool_call }) = events.recv().await else {
            panic!("expected the child's tool call");
        };
        assert_eq!(tool_call.id, "child-tool");
        assert_eq!(tool_call.parent_tool_call_id.as_deref(), Some("tool-1"));

        // The child's own announcement of itself is rejected (self-parenting),
        // so nothing else was published: no message, no stranger, no signal.
        assert!(events.recv().await.is_none());
        assert!(lifecycle.try_recv().is_err());
    }
}
