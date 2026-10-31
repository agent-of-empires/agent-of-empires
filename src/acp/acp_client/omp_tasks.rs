//! omp task progress is a rolling snapshot, independent of the spawning tool.

use std::collections::HashMap;

use agent_client_protocol::schema::v1::SessionUpdate;
use serde::Deserialize;

use crate::acp::state::{BackgroundAgentStatus, BackgroundAgentTool, Event};

#[derive(Default)]
pub(super) struct OmpTaskTracker {
    children: HashMap<String, Child>,
}

struct Child {
    snapshot: Option<Progress>,
    terminal: bool,
}

#[derive(Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct Progress {
    id: String,
    agent: String,
    status: String,
    #[serde(default)]
    assignment: Option<String>,
    #[serde(default)]
    task: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    resolved_model: Option<String>,
    #[serde(default)]
    tool_count: u32,
    #[serde(default)]
    recent_tools: Vec<RecentTool>,
    #[serde(default)]
    recent_output: Vec<String>,
    #[serde(default)]
    current_tool: Option<String>,
}

#[derive(Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct RecentTool {
    tool: String,
    #[serde(default)]
    args: String,
    #[serde(default)]
    is_error: Option<bool>,
}

impl Progress {
    fn status(&self) -> Option<(BackgroundAgentStatus, Option<&'static str>)> {
        Some(match self.status.as_str() {
            "pending" | "running" => (BackgroundAgentStatus::Running, None),
            "completed" => (BackgroundAgentStatus::Completed, None),
            "failed" => (BackgroundAgentStatus::Error, Some("omp task failed")),
            "aborted" | "cancelled" => (
                BackgroundAgentStatus::Error,
                Some("omp task cancelled or aborted"),
            ),
            _ => return None,
        })
    }

    fn tools(&self) -> Vec<BackgroundAgentTool> {
        self.recent_tools
            .iter()
            .map(|tool| BackgroundAgentTool {
                name: tool.tool.clone(),
                title: (!tool.args.is_empty()).then(|| tool.args.clone()),
                ok: Some(!tool.is_error.unwrap_or(false)),
            })
            .collect()
    }
}

pub(super) fn is_omp_task_snapshot(update: &SessionUpdate) -> bool {
    let SessionUpdate::ToolCallUpdate(update) = update else {
        return false;
    };
    update
        .fields
        .raw_output
        .as_ref()
        .and_then(|output| output.pointer("/details/async/type"))
        .and_then(|kind| kind.as_str())
        == Some("task")
}

impl OmpTaskTracker {
    pub(super) fn clear(&mut self) {
        self.children.clear();
    }

    pub(super) fn observe(&mut self, update: &SessionUpdate, replay: bool) -> Vec<Event> {
        if !is_omp_task_snapshot(update) {
            return Vec::new();
        }
        let SessionUpdate::ToolCallUpdate(update) = update else {
            return Vec::new();
        };
        let Some(rows) = update
            .fields
            .raw_output
            .as_ref()
            .and_then(|output| output.pointer("/details/progress"))
            .and_then(|progress| progress.as_array())
        else {
            return Vec::new();
        };
        let mut events = Vec::new();
        for row in rows {
            let Ok(mut progress) = Progress::deserialize(row) else {
                continue;
            };
            if progress.id.trim().is_empty() || progress.agent.trim().is_empty() {
                continue;
            }
            let Some((status, warning)) = progress.status() else {
                continue;
            };
            let terminal = status != BackgroundAgentStatus::Running;
            progress.recent_output.reverse();
            // Suppressed adapter history seeds identity without republishing it.
            if replay {
                let child = self.children.entry(progress.id.clone()).or_insert(Child {
                    snapshot: None,
                    terminal: false,
                });
                if !child.terminal || child.snapshot.is_none() && terminal {
                    child.snapshot = Some(progress);
                    child.terminal = terminal;
                }
                continue;
            }
            let child = self.children.entry(progress.id.clone()).or_insert_with(|| {
                events.push(Event::BackgroundAgentLaunched {
                    agent_id: progress.id.clone(),
                    tool_call_id: update.tool_call_id.0.to_string(),
                    description: progress
                        .description
                        .clone()
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| progress.id.clone()),
                    prompt: progress
                        .assignment
                        .clone()
                        .or_else(|| progress.task.clone())
                        .unwrap_or_default(),
                    model: progress.resolved_model.clone().unwrap_or_default(),
                    output_file: String::new(),
                    wire_tracked: true,
                    started_at: chrono::Utc::now(),
                });
                Child {
                    snapshot: None,
                    terminal: false,
                }
            });
            if child.terminal && !terminal || child.snapshot.as_ref() == Some(&progress) {
                continue;
            }
            if child.terminal && child.snapshot.is_none() {
                // Reattach seeds have no snapshot; preserve their completion time.
                child.snapshot = Some(progress);
                continue;
            }
            let completion_changed = terminal
                && (!child.terminal
                    || child.snapshot.as_ref().and_then(Progress::status)
                        != Some((status, warning)));
            let tools = progress.tools();
            events.push(Event::BackgroundAgentProgress {
                agent_id: progress.id.clone(),
                status,
                model: progress.resolved_model.clone(),
                tool_count: progress.tool_count,
                tools,
                last_tool: progress
                    .current_tool
                    .clone()
                    .or_else(|| progress.recent_tools.first().map(|tool| tool.tool.clone())),
                last_text: Some(progress.recent_output.join("\n")),
                at: chrono::Utc::now(),
            });
            if completion_changed {
                events.push(Event::BackgroundAgentCompleted {
                    agent_id: progress.id.clone(),
                    status,
                    tools: Vec::new(),
                    // Async final results are not present in omp's results array.
                    result: None,
                    warning: warning.map(str::to_string),
                    ended_at: chrono::Utc::now(),
                });
            }
            child.snapshot = Some(progress);
            child.terminal = terminal;
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::state::AcpState;
    use serde_json::{json, Value};

    fn update(rows: Value, status: &str) -> SessionUpdate {
        serde_json::from_value(json!({
            "sessionUpdate": "tool_call_update", "toolCallId": "batch",
            "status": status,
            "rawOutput": {"details": {"async": {"type": "task", "jobId": "A"}, "progress": rows}}
        }))
        .unwrap()
    }

    fn row(id: &str, status: &str) -> Value {
        json!({"id": id, "agent": "scout", "status": status,
            "assignment": "Inspect code", "resolvedModel": "provider/model",
            "toolCount": 3, "recentTools": [{"tool": "read", "args": "src", "isError": false}],
            "recentOutput": ["Found code"]})
    }

    #[test]
    fn omp_tasks_correct_terminal_status_without_duplicate_completion() {
        let mut tracker = OmpTaskTracker::default();
        let mut state = AcpState::default();
        let completed = update(json!([row("A", "completed")]), "in_progress");
        for event in tracker.observe(&completed, false) {
            state
                .apply_event(state.last_seq.saturating_add(1), event)
                .unwrap();
        }
        let corrected = update(json!([row("A", "failed")]), "completed");
        let events = tracker.observe(&corrected, false);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, Event::BackgroundAgentCompleted { .. }))
                .count(),
            1
        );
        assert!(!events.iter().any(|event| matches!(
            event,
            Event::BackgroundAgentLaunched { .. }
                | Event::BackgroundAgentProgress {
                    status: BackgroundAgentStatus::Running,
                    ..
                }
        )));
        for event in events {
            state
                .apply_event(state.last_seq.saturating_add(1), event)
                .unwrap();
        }
        assert_eq!(
            state.background_agents[0].status,
            BackgroundAgentStatus::Error
        );
        assert_eq!(
            state.background_agents[0].warning.as_deref(),
            Some("omp task failed")
        );
        assert!(tracker.observe(&corrected, false).is_empty());
        let running = update(json!([row("A", "running")]), "in_progress");
        assert!(tracker.observe(&running, false).is_empty());
        let mut refreshed = row("A", "failed");
        refreshed["recentOutput"] = json!(["Merge failed"]);
        let events = tracker.observe(&update(json!([refreshed]), "completed"), false);
        assert!(!events
            .iter()
            .any(|event| matches!(event, Event::BackgroundAgentCompleted { .. })));
        for event in tracker.observe(&completed, false) {
            state
                .apply_event(state.last_seq.saturating_add(1), event)
                .unwrap();
        }
        assert_eq!(
            state.background_agents[0].status,
            BackgroundAgentStatus::Completed
        );
        assert!(state.background_agents[0].warning.is_none());
        assert!(tracker.observe(&completed, false).is_empty());
    }

    #[test]
    fn omp_tasks_recent_output_is_chronological_without_reordering_lines() {
        let mut tracker = OmpTaskTracker::default();
        let mut progress = row("A", "running");
        progress["recentOutput"] = json!(["Newest first line\nNewest second line", "Oldest"]);
        let snapshot = update(json!([progress]), "completed");
        let events = tracker.observe(&snapshot, false);
        assert!(events.iter().any(|event| matches!(event,
            Event::BackgroundAgentProgress { last_text: Some(text), .. }
                if text == "Oldest\nNewest first line\nNewest second line")));
        assert!(tracker.observe(&snapshot, false).is_empty());
    }

    #[test]
    fn omp_tasks_use_names_unless_a_description_is_present() {
        let mut tracker = OmpTaskTracker::default();
        let mut described = row("SmokeBeta", "running");
        described["description"] = json!("Inspect backend");
        let events = tracker.observe(
            &update(
                json!([row("SmokeAlpha", "running"), described]),
                "completed",
            ),
            false,
        );
        let launches: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                Event::BackgroundAgentLaunched {
                    agent_id,
                    description,
                    ..
                } => Some((agent_id.as_str(), description.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(
            launches,
            [
                ("SmokeAlpha", "SmokeAlpha"),
                ("SmokeBeta", "Inspect backend")
            ]
        );
    }

    #[test]
    fn omp_tasks_last_tool_uses_newest_recent_tool_unless_current() {
        for current in [None, Some("bash")] {
            let mut tracker = OmpTaskTracker::default();
            let mut progress = row("A", "running");
            progress["recentTools"] = json!([
                {"tool": "grep", "args": "newest"},
                {"tool": "read", "args": "oldest"}
            ]);
            if let Some(tool) = current {
                progress["currentTool"] = json!(tool);
            }
            let events = tracker.observe(&update(json!([progress]), "completed"), false);
            let mut state = AcpState::default();
            for event in events {
                state
                    .apply_event(state.last_seq.saturating_add(1), event)
                    .unwrap();
            }
            assert_eq!(
                state.background_agents[0].last_tool.as_deref(),
                Some(current.unwrap_or("grep"))
            );
        }
    }

    #[test]
    fn omp_tasks_settle_independently_and_replace_snapshots() {
        let mut tracker = OmpTaskTracker::default();
        let initial = update(
            json!([row("A", "running"), row("B", "pending")]),
            "completed",
        );
        let mut state = AcpState::default();
        let events = tracker.observe(&initial, false);
        assert_eq!(events.iter().filter(|e| matches!(e, Event::BackgroundAgentLaunched { output_file, wire_tracked: true, .. } if output_file.is_empty())).count(), 2);
        for event in events {
            state
                .apply_event(state.last_seq.saturating_add(1), event)
                .unwrap();
        }
        assert!(tracker.observe(&initial, false).is_empty());
        assert_eq!(state.background_agents.len(), 2);
        assert!(state
            .background_agents
            .iter()
            .all(|a| a.status == BackgroundAgentStatus::Running));

        let mut next = row("B", "running");
        next["toolCount"] = json!(4);
        next["recentTools"] = json!([{"tool": "grep", "args": "pattern", "isError": true}]);
        next["recentOutput"] = json!(["New preview"]);
        let mixed = update(json!([row("A", "completed"), next]), "in_progress");
        let events = tracker.observe(&mixed, false);
        assert!(!events
            .iter()
            .any(|e| matches!(e, Event::BackgroundAgentLaunched { .. })));
        for event in events {
            state
                .apply_event(state.last_seq.saturating_add(1), event)
                .unwrap();
        }
        assert!(tracker.observe(&mixed, false).is_empty());
        assert_eq!(
            state.background_agents[0].status,
            BackgroundAgentStatus::Completed
        );
        let b = &state.background_agents[1];
        assert_eq!(b.status, BackgroundAgentStatus::Running);
        assert_eq!(b.tool_count, 4);
        assert_eq!(b.tools.len(), 1);
        assert_eq!(b.tools[0].name, "grep");
        assert_eq!(b.tools[0].ok, Some(false));
        assert_eq!(b.last_text.as_deref(), Some("New preview"));
        assert!(state.background_agents[0].result.is_none());
        assert!(tracker
            .observe(&initial, false)
            .iter()
            .all(|e| !matches!(e, Event::BackgroundAgentLaunched { .. })));
    }

    #[test]
    fn omp_tasks_ignore_malformed_rows_and_report_cancellation() {
        let mut tracker = OmpTaskTracker::default();
        let input = update(
            json!([null, {}, row("", "running"), row("X", "unknown"),
            {"id":"bad", "agent":"task", "status":"running", "toolCount": -1},
            row("A", "aborted"), row("B", "cancelled"), row("C", "failed")]),
            "failed",
        );
        let events = tracker.observe(&input, false);
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, Event::BackgroundAgentLaunched { .. }))
                .count(),
            3
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(
                    e,
                    Event::BackgroundAgentCompleted {
                        status: BackgroundAgentStatus::Error,
                        warning: Some(_),
                        ..
                    }
                ))
                .count(),
            3
        );
        assert!(tracker.observe(&input, false).is_empty());
        let unrelated: SessionUpdate = serde_json::from_value(json!({"sessionUpdate":"tool_call_update", "toolCallId":"other", "rawOutput":{"details":{"progress":[row("D","running")]}}})).unwrap();
        assert!(tracker.observe(&unrelated, false).is_empty());
    }
}
