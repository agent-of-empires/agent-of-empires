//! Which events belong in the durable transcript, and under what kind.

use crate::acp::state::Event;

/// True when the event would reproduce a prior turn's visible
/// transcript. Used to scope the post-`session/load` suppression
/// window: claude-agent-acp re-emits historical assistant chunks and
/// tool calls during the load handshake (which would double-render
/// against our own SQLite-restored transcript), but it ALSO emits
/// ambient state (available_commands, current_mode, usage) and
/// lifecycle events that the UI needs immediately on resume. Drop the
/// former, pass the latter through.
pub(super) fn is_transcript_event(event: &Event) -> bool {
    matches!(
        event,
        Event::AgentMessageChunk { .. }
            | Event::ToolCallStarted { .. }
            | Event::ToolCallCompleted { .. }
            | Event::ToolCallContent { .. }
            | Event::ToolCallUpdated { .. }
            | Event::DiffEmitted { .. }
            | Event::PlanUpdated { .. }
            | Event::TodoListUpdated { .. }
            | Event::ThinkingStarted
            | Event::ThinkingEnded
            | Event::UserPromptSent { .. }
            | Event::UserDiffCommentsPrompt { .. }
            | Event::ApprovalRequested { .. }
            | Event::ApprovalResolved { .. }
            | Event::RawAgentUpdate { .. }
            // A replayed launch must be dropped too: the tailer spawn is
            // skipped during suppression, so letting it through would
            // create a running record with nothing to ever complete it.
            | Event::BackgroundAgentLaunched { .. }
            | Event::PromptRuntimeError { .. }
            // A replayed completion would re-run its side effects on every
            // reattach (a duplicate divider and summary, a re-based web
            // `usageBaseline`), and a replayed start would latch
            // `compacting` for a summarization the reloaded adapter cannot
            // resume (#3219).
            | Event::ConversationCompactionStarted
            | Event::ConversationCompacted
            | Event::ConversationCompactionFailed
            | Event::ConversationCompactionSummary { .. }
    )
}

/// Cheap discriminant for log breadcrumbs (matches the one in
/// event_store, kept separate so this module doesn't depend on the
/// store's private helper).
pub(super) fn transcript_event_kind(event: &Event) -> &'static str {
    match event {
        Event::AgentMessageChunk { .. } => "agent_message_chunk",
        Event::ToolCallStarted { .. } => "tool_call_started",
        Event::ToolCallCompleted { .. } => "tool_call_completed",
        Event::ToolCallContent { .. } => "tool_call_content",
        Event::ToolCallUpdated { .. } => "tool_call_updated",
        Event::DiffEmitted { .. } => "diff_emitted",
        Event::PlanUpdated { .. } => "plan_updated",
        Event::TodoListUpdated { .. } => "todo_list_updated",
        Event::ThinkingStarted => "thinking_started",
        Event::ThinkingEnded => "thinking_ended",
        Event::UserPromptSent { .. } => "user_prompt_sent",
        Event::UserDiffCommentsPrompt { .. } => "user_diff_comments_prompt",
        Event::ApprovalRequested { .. } => "approval_requested",
        Event::ApprovalResolved { .. } => "approval_resolved",
        Event::RawAgentUpdate { .. } => "raw_agent_update",
        Event::PromptRuntimeError { .. } => "prompt_runtime_error",
        Event::ConversationCompactionStarted => "conversation_compaction_started",
        Event::ConversationCompacted => "conversation_compacted",
        Event::ConversationCompactionFailed => "conversation_compaction_failed",
        Event::ConversationCompactionSummary { .. } => "conversation_compaction_summary",
        _ => "other",
    }
}
