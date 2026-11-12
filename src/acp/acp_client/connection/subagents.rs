//! Subagent sessions: the ACP subagents RFD's child sessions, which share the
//! parent's connection and arrive addressed to their own session id.

use agent_client_protocol::schema::MaybeUndefined;
use agent_client_protocol::v1::schema::{
    ContentBlock, SessionId, SessionMessage, SessionUpdate, StateUpdate, SubagentUpdate,
};

use super::notifications::{now_ms, Shared};
use crate::acp::acp_client::config_options::map_acp_config_option;
use crate::acp::acp_client::lifecycle::ASYNC_AGENT_LAUNCHED;
use crate::acp::acp_client::tool_context::update_tool_context_cache;
use crate::acp::acp_client::tool_output::preview_args;
use crate::acp::acp_client::update_events::map_update_to_events;
use crate::acp::acp_client::watchdog::OFF_PROTOCOL_WORK_GRACE_FLOOR;
use crate::acp::state::{
    ConfigOptionCategory, Event, SubagentRecord, SubagentState, ToolCall, ToolOutputBlock,
};

/// A background child's card, open until the child goes idle, and the text
/// of its latest message: its answer once it ends.
#[derive(Debug)]
pub(super) struct BackgroundReport {
    tool_call_id: String,
    message_id: Option<String>,
    text: String,
}

fn patch<T>(slot: &mut Option<T>, incoming: MaybeUndefined<T>) {
    match incoming {
        MaybeUndefined::Undefined => {}
        MaybeUndefined::Null => *slot = None,
        MaybeUndefined::Value(value) => *slot = Some(value),
    }
}

fn apply_state(record: &mut SubagentRecord, state: MaybeUndefined<StateUpdate>) {
    let (state, stop) = match state {
        MaybeUndefined::Undefined => return,
        MaybeUndefined::Null => (SubagentState::Unknown, None),
        MaybeUndefined::Value(StateUpdate::Running(_)) => (SubagentState::Running, None),
        MaybeUndefined::Value(StateUpdate::RequiresAction(_)) => {
            (SubagentState::RequiresAction, None)
        }
        MaybeUndefined::Value(StateUpdate::Idle(idle)) => {
            // The reason is an open string: the RFD reserves future values.
            let stop = idle
                .stop_reason
                .and_then(|reason| serde_json::to_value(reason).ok());
            (SubagentState::Idle, stop)
        }
        MaybeUndefined::Value(_) => (SubagentState::Unknown, None),
    };
    let field = |key: &str| {
        let value = stop.as_ref()?.get(key)?;
        value.as_str().map(str::to_owned)
    };
    record.state = state;
    record.stop_reason = field("stopReason");
    record.error = stop.as_ref().and_then(|stop| {
        stop.get("error")?
            .get("message")?
            .as_str()
            .map(str::to_owned)
    });
}

impl Shared {
    /// A child still holds its parent's turn open. One silent past the
    /// off-protocol floor lost its idle update, so it stops counting, whatever
    /// its siblings send.
    pub(super) fn subagent_working(&self, now_ms: i64) -> bool {
        let floor = OFF_PROTOCOL_WORK_GRACE_FLOOR.as_millis() as i64;
        let seen = self
            .subagent_seen
            .lock()
            .expect("subagent seen mutex poisoned");
        self.subagents
            .lock()
            .expect("subagents mutex poisoned")
            .iter()
            .any(|(id, record)| {
                matches!(
                    record.state,
                    SubagentState::Running | SubagentState::RequiresAction
                ) && seen.get(id).is_some_and(|at| now_ms - at < floor)
            })
    }

    fn mark_child_seen(&self, child: &SessionId) {
        self.subagent_seen
            .lock()
            .expect("subagent seen mutex poisoned")
            .insert(child.0.to_string(), now_ms());
    }

    /// A running child would lose its routes if the conversation reset.
    pub(super) fn has_active_subagent(&self) -> bool {
        self.subagents
            .lock()
            .expect("subagents mutex poisoned")
            .values()
            .any(|record| {
                matches!(
                    record.state,
                    SubagentState::Running | SubagentState::RequiresAction
                )
            })
    }

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
                    error: None,
                    cancel_capable: false,
                    model: None,
                    background: false,
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

    /// `parent` is the session the announcement arrived on. Returns whether
    /// it was accepted.
    pub(super) async fn handle_subagent_update(
        &self,
        parent: SessionId,
        update: SubagentUpdate,
    ) -> bool {
        let child = update.session_id.clone();
        // Ingress registers only announcements that name a valid child of the
        // session they arrived on; anything else is dropped here.
        if self
            .ingress
            .child_route(&child)
            .is_none_or(|route| route.parent != parent)
        {
            return false;
        }
        self.publish_subagent(&parent, &child, |record| {
            record.parent_session_id = parent.0.to_string();
            patch(&mut record.title, update.title);
            patch(&mut record.description, update.description);
            apply_state(record, update.state);
        })
        .await;
        self.mark_child_seen(&child);
        self.finish_background_report(&child).await;
        true
    }

    /// An idle background child's answer, or its error, completes its card.
    /// A cancelled or orphaned one leaves it open for the turn's end to mark stopped.
    async fn finish_background_report(&self, child: &SessionId) {
        let (stop_reason, error) = {
            let subagents = self.subagents.lock().expect("subagents mutex poisoned");
            match subagents.get(child.0.as_ref()) {
                Some(record) if record.state == SubagentState::Idle => {
                    (record.stop_reason.clone(), record.error.clone())
                }
                _ => return,
            }
        };
        // Any other stop keeps the report for a later end_turn or error.
        let is_error = match stop_reason.as_deref() {
            Some("end_turn") => false,
            Some("error") => true,
            _ => return,
        };
        let Some(report) = self
            .background_reports
            .lock()
            .expect("background reports mutex poisoned")
            .remove(child.0.as_ref())
        else {
            return;
        };
        let text = match error {
            Some(error) if is_error => error,
            _ => report.text,
        };
        self.emit_card_completion(report.tool_call_id, text, is_error)
            .await;
    }

    /// `session` is the session the message arrived on. The adapter hides the
    /// Agent call, so the delegation prompt (parent to child) becomes the card
    /// that nests the child's tool calls, and the child's reply or hand-back
    /// report (child to parent) completes it. Other messages are dropped;
    /// returns whether this one was accepted.
    pub(super) async fn handle_session_message(
        &self,
        session: SessionId,
        message: SessionMessage,
    ) -> bool {
        let (MaybeUndefined::Value(sender), MaybeUndefined::Value(recipient)) =
            (&message.sender_session_id, &message.recipient_session_id)
        else {
            return false;
        };
        let child_of_session = |id: &SessionId| {
            self.ingress
                .child_route(id)
                .is_some_and(|route| route.parent == session)
        };
        let text: String = match &message.content {
            MaybeUndefined::Value(blocks) => blocks
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text(text) => Some(text.text.as_str()),
                    _ => None,
                })
                .collect(),
            _ => String::new(),
        };
        if *sender == session && child_of_session(recipient) {
            self.mark_child_seen(recipient);
            self.start_delegation_card(&session, recipient, message.message_id.0.to_string(), text)
                .await;
        } else if *recipient == session && child_of_session(sender) {
            self.mark_child_seen(sender);
            self.complete_delegation_card(sender, &message.message_id.0, text)
                .await;
        } else {
            return false;
        }
        true
    }

    /// The first parent-to-child message links the child to its launch. Later
    /// ones are follow-ups, not new launches.
    async fn start_delegation_card(
        &self,
        parent: &SessionId,
        child: &SessionId,
        message_id: String,
        prompt: String,
    ) {
        let (title, description, outer) = {
            let subagents = self.subagents.lock().expect("subagents mutex poisoned");
            let Some(record) = subagents.get(child.0.as_ref()) else {
                return;
            };
            if record.parent_tool_call_id.is_some() {
                return;
            }
            (
                record.title.clone(),
                record.description.clone(),
                subagents
                    .get(parent.0.as_ref())
                    .and_then(|record| record.parent_tool_call_id.clone()),
            )
        };
        let name = title.clone().or(description.clone()).unwrap_or_default();
        let args = serde_json::json!({ "description": description.or(title), "prompt": prompt });
        self.emit(Event::ToolCallStarted {
            tool_call: ToolCall {
                id: message_id.clone(),
                name,
                kind: "think".into(),
                args_preview: preview_args(&args),
                prompt: None,
                started_at: chrono::Utc::now(),
                parent_tool_call_id: outer,
                memory_recall: None,
                diffs: Vec::new(),
            },
        })
        .await;
        self.publish_subagent(parent, child, |record| {
            record.parent_tool_call_id = Some(message_id);
        })
        .await;
    }

    async fn complete_delegation_card(&self, child: &SessionId, message_id: &str, reply: String) {
        let Some(tool_call_id) = self
            .subagents
            .lock()
            .expect("subagents mutex poisoned")
            .get(child.0.as_ref())
            .and_then(|record| record.parent_tool_call_id.clone())
        else {
            return;
        };
        // The adapter sends an async launch's acknowledgement as its reply
        // when the Agent call lacks `run_in_background`.
        let launch_notice = message_id == format!("{tool_call_id}:reply")
            && reply
                .lines()
                .any(|line| line.trim_start().starts_with(ASYNC_AGENT_LAUNCHED));
        if !launch_notice {
            return self.emit_card_completion(tool_call_id, reply, false).await;
        }
        self.background_reports
            .lock()
            .expect("background reports mutex poisoned")
            .insert(
                child.0.to_string(),
                BackgroundReport {
                    tool_call_id,
                    message_id: None,
                    text: String::new(),
                },
            );
        let parent = self.ingress.child_route(child).map(|route| route.parent);
        if let Some(parent) = parent {
            self.publish_subagent(&parent, child, |record| record.background = true)
                .await;
        }
    }

    async fn emit_card_completion(&self, tool_call_id: String, reply: String, is_error: bool) {
        self.emit(Event::ToolCallCompleted {
            tool_call_id,
            is_error,
            output: vec![ToolOutputBlock::Text {
                text: reply.clone(),
            }],
            content: reply,
            completed_at: chrono::Utc::now(),
            async_subagent: false,
        })
        .await;
    }

    /// Only a child's tool calls and model reach the transcript; its own
    /// lifecycle must never read as the root session's. A background child's
    /// latest message is kept as its answer.
    pub(super) async fn handle_child_notification(&self, child: SessionId, update: SessionUpdate) {
        self.mark_child_seen(&child);
        match update {
            SessionUpdate::SubagentUpdate(nested) => {
                self.handle_subagent_update(child, nested).await;
            }
            SessionUpdate::AgentMessageChunk(chunk) => {
                let ContentBlock::Text(text) = chunk.content else {
                    return;
                };
                let id = chunk.message_id.map(|id| id.0.to_string());
                let mut reports = self
                    .background_reports
                    .lock()
                    .expect("background reports mutex poisoned");
                if let Some(report) = reports.get_mut(child.0.as_ref()) {
                    if id.is_some() && id != report.message_id {
                        report.message_id = id;
                        report.text.clear();
                    }
                    report.text.push_str(&text.text);
                }
            }
            tool @ (SessionUpdate::ToolCall(_) | SessionUpdate::ToolCallUpdate(_)) => {
                // Prose before a tool call is not the child's answer.
                if matches!(tool, SessionUpdate::ToolCall(_)) {
                    if let Some(report) = self
                        .background_reports
                        .lock()
                        .expect("background reports mutex poisoned")
                        .get_mut(child.0.as_ref())
                    {
                        report.text.clear();
                    }
                }
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
                    update_tool_context_cache(&self.tool_context_cache, &mut event, &source);
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
    use crate::acp::acp_client::lifecycle::LifecycleSignal;
    use crate::acp::acp_client::session_identity::SessionIngress;
    use crate::acp::acp_client::SessionState;
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
            },
        }))
    }

    /// The parent-view delegation message `linkedSessionMessages` publishes.
    fn delegation(id: &str, child: &str, text: &str) -> serde_json::Value {
        serde_json::json!({
            "sessionId": "root",
            "update": {
                "sessionUpdate": "session_message",
                "messageId": id,
                "senderSessionId": "root",
                "recipientSessionId": child,
                "recipientMessageId": id,
                "content": [{ "type": "text", "text": text }],
            },
        })
    }

    /// A child's message to its parent: a foreground reply (`<launch>:reply`)
    /// or a hand-back report.
    fn to_parent(id: &str, sender: &str, text: &str) -> serde_json::Value {
        serde_json::json!({
            "sessionId": "root",
            "update": {
                "sessionUpdate": "session_message",
                "messageId": id,
                "senderSessionId": sender,
                "recipientSessionId": "root",
                "content": [{ "type": "text", "text": text }],
            },
        })
    }

    fn harness() -> (Arc<SessionIngress>, Shared, mpsc::Receiver<Event>) {
        let ingress = Arc::new(SessionIngress::new(Some("root".to_string().into())));
        let (event_tx, events) = mpsc::channel(16);
        let (lifecycle_tx, _lifecycle) = mpsc::channel(16);
        let shared = Shared::new(
            event_tx,
            "test".into(),
            &CLAUDE,
            lifecycle_tx,
            Arc::new(SessionState::default()),
            Arc::new(AtomicBool::new(false)),
            None,
            ingress.clone(),
            None,
        );
        (ingress, shared, events)
    }

    async fn drain(shared: Shared, mut events: mpsc::Receiver<Event>) -> Vec<Event> {
        drop(shared);
        let mut all = Vec::new();
        while let Some(event) = events.recv().await {
            all.push(event);
        }
        all
    }

    /// The announcement precedes the prompt pair: the delegation message
    /// becomes the card and patches the link into a full snapshot. The
    /// child's own view of the prompt and a later follow-up add nothing.
    #[tokio::test]
    async fn delegation_message_links_the_child_and_starts_its_card() {
        let (ingress, shared, events) = harness();
        feed(
            &ingress,
            &shared,
            serde_json::to_value(announce("root", "running")).unwrap(),
        )
        .await;
        feed(
            &ingress,
            &shared,
            delegation("tool-1", "child", "find the bug"),
        )
        .await;
        let mut child_view = delegation("tool-1", "child", "find the bug");
        child_view["sessionId"] = "child".into();
        child_view["update"]["senderMessageId"] = "tool-1".into();
        feed(&ingress, &shared, child_view).await;
        feed(
            &ingress,
            &shared,
            delegation("send-2", "child", "and another"),
        )
        .await;

        let events = drain(shared, events).await;
        let [Event::SubagentUpdated {
            subagent: first, ..
        }, Event::ToolCallStarted { tool_call }, Event::SubagentUpdated {
            subagent: linked, ..
        }] = events.as_slice()
        else {
            panic!("unexpected events: {events:?}");
        };
        assert_eq!(first.parent_tool_call_id, None);
        assert_eq!(tool_call.id, "tool-1");
        assert_eq!(tool_call.name, "Explore");
        assert_eq!(tool_call.kind, "think");
        assert_eq!(tool_call.parent_tool_call_id, None);
        assert!(tool_call.args_preview.contains("find the bug"));
        assert_eq!(linked.parent_tool_call_id.as_deref(), Some("tool-1"));
        assert_eq!(linked.title.as_deref(), Some("Explore"));
        assert!(linked.cancel_capable);
        assert_eq!(linked.state, SubagentState::Running);
    }

    /// A background launch's acknowledgement is not the child's answer: the
    /// card stays open, marked background, and completes with the child's
    /// last message once it ends. A cancelled child leaves it open (#5612).
    #[tokio::test]
    async fn background_launch_completes_with_the_childs_answer() {
        let notice = "Async agent launched successfully. (internal metadata) agentId: a1";
        let chunk = |id: &str, text: &str| {
            serde_json::json!({
                "sessionId": "child",
                "update": { "sessionUpdate": "agent_message_chunk", "messageId": id,
                            "content": { "type": "text", "text": text } },
            })
        };
        for (stop, want) in [
            ("end_turn", Some(("The answer is 42.", false))),
            ("error", Some(("Authentication required", true))),
            ("cancelled", None),
        ] {
            let (ingress, shared, events) = harness();
            for frame in [
                serde_json::to_value(announce("root", "running")).unwrap(),
                delegation("tool-1", "child", "dig"),
                to_parent("tool-1:reply", "child", notice),
                chunk("m1", "Let me look."),
                serde_json::json!({
                    "sessionId": "child",
                    "update": { "sessionUpdate": "tool_call", "toolCallId": "child-tool",
                                "title": "Read", "kind": "read", "status": "pending" },
                }),
                chunk("m2", "The answer"),
                chunk("m2", " is 42."),
                // An idle snapshot with no reason is not the outcome yet.
                serde_json::json!({
                    "sessionId": "root",
                    "update": { "sessionUpdate": "subagent_update", "sessionId": "child",
                                "state": { "state": "idle" } },
                }),
                serde_json::json!({
                    "sessionId": "root",
                    "update": { "sessionUpdate": "subagent_update", "sessionId": "child",
                                "state": { "state": "idle", "stopReason": stop,
                                           "error": { "code": -32000, "message": "Authentication required" } } },
                }),
            ] {
                feed(&ingress, &shared, frame).await;
            }
            let events = drain(shared, events).await;
            assert!(
                events.iter().any(|e| matches!(
                    e,
                    Event::SubagentUpdated { subagent, .. } if subagent.background
                )),
                "{stop}: no background snapshot in {events:?}"
            );
            let completions: Vec<_> = events
                .iter()
                .filter_map(|e| match e {
                    Event::ToolCallCompleted {
                        tool_call_id,
                        content,
                        is_error,
                        ..
                    } if tool_call_id == "tool-1" => Some((content.as_str(), *is_error)),
                    _ => None,
                })
                .collect();
            match want {
                Some(completion) => assert_eq!(completions, [completion], "{stop}"),
                None => assert!(completions.is_empty(), "{stop}: {completions:?}"),
            }
        }

        // Only the launch's own reply is an acknowledgement.
        let (ingress, shared, events) = harness();
        feed(
            &ingress,
            &shared,
            serde_json::to_value(announce("root", "running")).unwrap(),
        )
        .await;
        feed(&ingress, &shared, delegation("tool-1", "child", "dig")).await;
        feed(&ingress, &shared, to_parent("handback-1", "child", notice)).await;
        let events = drain(shared, events).await;
        assert!(events.iter().any(|e| matches!(
            e,
            Event::ToolCallCompleted { content, .. } if content == notice
        )));
    }

    /// A foreground reply and a hand-back report both complete the card with
    /// the child's text; a stranger's message or one before the link does not.
    #[tokio::test]
    async fn child_message_to_the_parent_completes_the_card() {
        let (ingress, shared, events) = harness();
        feed(
            &ingress,
            &shared,
            serde_json::to_value(announce("root", "running")).unwrap(),
        )
        .await;
        feed(
            &ingress,
            &shared,
            to_parent("tool-1:reply", "child", "too early"),
        )
        .await;
        feed(&ingress, &shared, delegation("tool-1", "child", "go")).await;
        feed(
            &ingress,
            &shared,
            to_parent("tool-1:reply", "stranger", "forged"),
        )
        .await;
        feed(
            &ingress,
            &shared,
            to_parent("tool-1:reply", "child", "the answer"),
        )
        .await;
        feed(
            &ingress,
            &shared,
            to_parent("handback-9", "child", "the report"),
        )
        .await;

        let events = drain(shared, events).await;
        let completions: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                Event::ToolCallCompleted {
                    tool_call_id,
                    content,
                    output,
                    is_error,
                    ..
                } => Some((
                    tool_call_id.as_str(),
                    content.as_str(),
                    output.len(),
                    *is_error,
                )),
                _ => None,
            })
            .collect();
        assert_eq!(
            completions,
            [
                ("tool-1", "the answer", 1, false),
                ("tool-1", "the report", 1, false)
            ]
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, Event::ToolCallStarted { .. }))
                .count(),
            1
        );
    }

    /// A message naming a child nobody announced creates no card, link or
    /// route, so a later announcement still starts unlinked.
    #[tokio::test]
    async fn delegation_for_an_unannounced_child_is_ignored() {
        let (ingress, shared, events) = harness();
        feed(&ingress, &shared, delegation("tool-1", "child", "early")).await;
        feed(
            &ingress,
            &shared,
            delegation("tool-2", "stranger", "nobody"),
        )
        .await;
        feed(
            &ingress,
            &shared,
            serde_json::to_value(announce("root", "running")).unwrap(),
        )
        .await;

        let events = drain(shared, events).await;
        let [Event::SubagentUpdated { subagent, .. }] = events.as_slice() else {
            panic!("unexpected events: {events:?}");
        };
        assert_eq!(subagent.parent_tool_call_id, None);
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
            Arc::new(SessionState::default()),
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

    /// A child silent past the floor stops holding the turn even while a
    /// sibling keeps sending traffic.
    #[tokio::test]
    async fn a_silent_child_stops_holding_the_turn_whatever_its_siblings_send() {
        let (_ingress, shared, _events) = harness();
        let floor = OFF_PROTOCOL_WORK_GRACE_FLOOR.as_millis() as i64;
        let now = now_ms();
        let child = |id: &str, state| SubagentRecord {
            session_id: id.into(),
            parent_session_id: "root".into(),
            parent_tool_call_id: None,
            title: None,
            description: None,
            state,
            stop_reason: None,
            error: None,
            cancel_capable: true,
            model: None,
            background: false,
        };
        for (id, state, seen) in [
            ("stale", SubagentState::Running, now - floor - 1),
            ("sibling", SubagentState::Idle, now),
        ] {
            shared
                .subagents
                .lock()
                .unwrap()
                .insert(id.into(), child(id, state));
            shared.subagent_seen.lock().unwrap().insert(id.into(), seen);
        }
        assert!(!shared.subagent_working(now));
        shared
            .subagent_seen
            .lock()
            .unwrap()
            .insert("stale".into(), now);
        assert!(shared.subagent_working(now));
    }

    /// A child's tool calls nest under the Agent card that spawned it, while
    /// its messages and lifecycle never reach the root session's transcript.
    /// Accepted child traffic keeps the parent's turn live for whichever
    /// watchdog owns it (#5611).
    #[tokio::test]
    async fn child_traffic_nests_tool_calls_and_keeps_the_parent_turn_live() {
        for in_flight in [false, true] {
            let ingress = Arc::new(SessionIngress::new(Some("root".to_string().into())));
            let (event_tx, mut events) = mpsc::channel(16);
            let (lifecycle_tx, mut lifecycle) = mpsc::channel(16);
            let shared = Shared::new(
                event_tx,
                "test".into(),
                &CLAUDE,
                lifecycle_tx,
                Arc::new(SessionState::default()),
                Arc::new(AtomicBool::new(in_flight)),
                None,
                ingress.clone(),
                None,
            );
            shared.prompt_epoch.store(1, Ordering::Relaxed);
            feed(
                &ingress,
                &shared,
                serde_json::to_value(announce("root", "running")).unwrap(),
            )
            .await;
            feed(
                &ingress,
                &shared,
                delegation("tool-1", "child", "look around"),
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
            let fed = now_ms();
            if in_flight {
                // Announcement, delegation, tool call and prose; the stranger and
                // the self-announcement are rejected.
                for _ in 0..4 {
                    let env = lifecycle.try_recv().expect("child activity signal");
                    assert_eq!(env.epoch, 1);
                    assert!(matches!(env.signal, LifecycleSignal::SubagentActivity));
                }
            } else {
                let active = shared.subagent_working(fed);
                assert!(active);
                let between = &shared.between_prompt;
                assert_eq!(between.take_idle_fire(fed + 120_000, false, active), None);
                let floor = OFF_PROTOCOL_WORK_GRACE_FLOOR.as_millis() as i64;
                assert_eq!(
                    between.take_idle_fire(fed + floor, false, active),
                    Some(false)
                );
            }
            assert!(lifecycle.try_recv().is_err());
            drop(shared);

            let Some(Event::SubagentUpdated { subagent, .. }) = events.recv().await else {
                panic!("expected the announcement first");
            };
            assert_eq!(subagent.session_id, "child");
            assert_eq!(subagent.parent_session_id, "root");
            assert_eq!(subagent.parent_tool_call_id, None);
            assert_eq!(subagent.title.as_deref(), Some("Explore"));
            assert_eq!(subagent.state, SubagentState::Running);
            assert!(subagent.cancel_capable);

            let Some(Event::ToolCallStarted { tool_call: card }) = events.recv().await else {
                panic!("expected the delegation card");
            };
            assert_eq!(card.id, "tool-1");
            let Some(Event::SubagentUpdated { subagent, .. }) = events.recv().await else {
                panic!("expected the linked snapshot");
            };
            assert_eq!(subagent.parent_tool_call_id.as_deref(), Some("tool-1"));

            let Some(Event::ToolCallStarted { tool_call }) = events.recv().await else {
                panic!("expected the child's tool call");
            };
            assert_eq!(tool_call.id, "child-tool");
            assert_eq!(tool_call.parent_tool_call_id.as_deref(), Some("tool-1"));

            // The child's own announcement of itself is rejected (self-parenting),
            // so nothing else was published: no message, no stranger, no signal.
            assert!(events.recv().await.is_none());
        }
    }
}
