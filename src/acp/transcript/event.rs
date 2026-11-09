use chrono::{DateTime, Utc};

use super::storage::{Backend, InputTool, Memory};
use crate::acp::state::Event;

macro_rules! event_view {
    ($lifetime:lifetime, $backend:ident; $($variant:ident $({$($field:ident: $ty:ty),* $(,)?})?),* $(,)?) => {
        pub(crate) enum View<$lifetime, $backend: Backend> {
            $($variant $({$($field: $ty),*})?),*,
            Other,
        }
        const PAYLOAD_KINDS: &[&str] = &[$(stringify!($variant)),*];
        const _: () = assert!(PAYLOAD_KINDS.len() < 128);
    };
}

event_view! { 'a, B;
    AgentMessageChunk { text: B::Input<'a>, message_id: Option<B::Input<'a>> },
    AgentThoughtChunk { text: B::Input<'a>, message_id: Option<B::Input<'a>> },
    UserPromptSent { text: B::Input<'a>, attachments: B::AttachmentsInput<'a>, prompt_id: Option<B::Input<'a>>, synthesized: bool, origin_plugin: Option<B::Input<'a>> },
    UserDiffCommentsPrompt { intro: B::Input<'a>, outro: B::Input<'a>, is_multi_repo: bool, comments: B::CommentsInput<'a>, assembled_markdown: B::Input<'a> },
    ToolCallStarted { tool_call: InputTool<'a, B> },
    ToolCallCompleted { tool_call_id: B::Input<'a>, is_error: bool, content: B::Input<'a>, output: B::OutputInput<'a>, completed_at: DateTime<Utc>, async_subagent: bool },
    ToolCallContent { tool_call_id: B::Input<'a>, content: B::Input<'a> },
    ToolCallUpdated { tool_call_id: B::Input<'a>, title: Option<B::Input<'a>>, args_preview: Option<B::Input<'a>>, started_at: Option<DateTime<Utc>>, diffs: Option<B::DiffsInput<'a>> },
    ElicitationRequested { tool_call_id: Option<B::Input<'a>> },
    ElicitationResolved { nonce: B::Input<'a>, answers: B::AnswersInput<'a> },
    Stopped,
    AgentStartupError { message: B::Input<'a> },
    IncompatibleAgent,
    AgentSwitched { from: B::Input<'a>, to: B::Input<'a>, reason: B::Input<'a> },
    SessionCleared,
    ConversationCompacted,
    ConversationCompactionSummary { compaction_id: B::Input<'a>, text: B::Input<'a> },
    SessionContextReset { reason: B::Input<'a> },
    PromptRuntimeError { message: B::Input<'a> },
    ModeSwitchFailed { mode_id: B::Input<'a>, reason: B::Input<'a> },
    SessionNotice { severity: B::Input<'a>, title: B::Input<'a>, description: Option<B::Input<'a>>, key: Option<B::Input<'a>> },
    RateLimitAutoResumed { resets_at: DateTime<Utc>, manual: bool },
    ConversationSummary { text: B::Input<'a> },
    ThinkingStarted,
    PromptCapabilities { steering: bool },
}

/// Withhold non-timeline metadata, including private host transcript paths.
pub(crate) fn has_retained_payload<S: pulldown_cmark::disk::ByteSource + ?Sized>(
    source: &S,
) -> std::io::Result<bool> {
    use pulldown_cmark::disk::Cursor;
    use std::io::{self, ErrorKind};
    let invalid = || io::Error::new(ErrorKind::InvalidData, "invalid retained event header");
    let mut cursor = Cursor::new(source, 0..source.len())?;
    let whitespace = |cursor: &mut Cursor<'_, S>| -> io::Result<()> {
        while cursor
            .peek()?
            .is_some_and(|byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
        {
            cursor.next_byte()?;
        }
        Ok(())
    };
    whitespace(&mut cursor)?;
    match cursor.next_byte()? {
        Some(b'{') => {
            whitespace(&mut cursor)?;
            if cursor.next_byte()? != Some(b'"') {
                return Err(invalid());
            }
        }
        Some(b'"') => {}
        _ => return Err(invalid()),
    }
    let mut candidates = u128::MAX >> (128 - PAYLOAD_KINDS.len());
    let mut position = 0;
    loop {
        let mut byte = cursor.next_byte()?.ok_or_else(invalid)?;
        if byte == b'"' {
            return Ok(PAYLOAD_KINDS.iter().enumerate().any(|(index, kind)| {
                candidates & (1u128 << index) != 0 && kind.len() == position
            }));
        }
        if byte == b'\\' {
            byte = cursor.next_byte()?.ok_or_else(invalid)?;
            if byte == b'u' {
                let mut value = 0u16;
                for _ in 0..4 {
                    let digit = cursor.next_byte()?.ok_or_else(invalid)?;
                    value = value * 16 + char::from(digit).to_digit(16).ok_or_else(invalid)? as u16;
                }
                if value > 127 {
                    return Ok(false);
                }
                byte = value as u8;
            } else {
                byte = match byte {
                    b'"' | b'\\' | b'/' => byte,
                    b'b' => 8,
                    b'f' => 12,
                    b'n' => b'\n',
                    b'r' => b'\r',
                    b't' => b'\t',
                    _ => return Err(invalid()),
                };
            }
        } else if byte < 32 {
            return Err(invalid());
        }
        for (index, kind) in PAYLOAD_KINDS.iter().enumerate() {
            if kind.as_bytes().get(position) != Some(&byte) {
                candidates &= !(1u128 << index);
            }
        }
        if candidates == 0 {
            return Ok(false);
        }
        position += 1;
    }
}

pub(crate) fn memory(event: &Event) -> View<'_, Memory> {
    match event {
        Event::AgentMessageChunk { text, message_id } => View::AgentMessageChunk {
            text,
            message_id: message_id.as_deref(),
        },
        Event::AgentThoughtChunk { text, message_id } => View::AgentThoughtChunk {
            text,
            message_id: message_id.as_deref(),
        },
        Event::UserPromptSent {
            text,
            attachments,
            prompt_id,
            synthesized,
            origin_plugin,
        } => View::UserPromptSent {
            text,
            attachments,
            prompt_id: prompt_id.as_deref(),
            synthesized: *synthesized,
            origin_plugin: origin_plugin.as_deref(),
        },
        Event::UserDiffCommentsPrompt {
            intro,
            outro,
            is_multi_repo,
            comments,
            assembled_markdown,
        } => View::UserDiffCommentsPrompt {
            intro,
            outro,
            is_multi_repo: *is_multi_repo,
            comments,
            assembled_markdown,
        },
        Event::ToolCallStarted { tool_call } => View::ToolCallStarted {
            tool_call: Memory.tool_input(tool_call),
        },
        Event::ToolCallCompleted {
            tool_call_id,
            is_error,
            content,
            output,
            completed_at,
            async_subagent,
        } => View::ToolCallCompleted {
            tool_call_id,
            is_error: *is_error,
            content,
            output,
            completed_at: *completed_at,
            async_subagent: *async_subagent,
        },
        Event::ToolCallContent {
            tool_call_id,
            content,
        } => View::ToolCallContent {
            tool_call_id,
            content,
        },
        Event::ToolCallUpdated {
            tool_call_id,
            title,
            args_preview,
            started_at,
            diffs,
        } => View::ToolCallUpdated {
            tool_call_id,
            title: title.as_deref(),
            args_preview: args_preview.as_deref(),
            started_at: *started_at,
            diffs: diffs.as_deref(),
        },
        Event::ElicitationRequested { elicitation } => View::ElicitationRequested {
            tool_call_id: elicitation.tool_call_id.as_deref(),
        },
        Event::ElicitationResolved { nonce, answers, .. } => View::ElicitationResolved {
            nonce: &nonce.0,
            answers,
        },
        Event::Stopped { .. } => View::Stopped,
        Event::AgentStartupError { message } => View::AgentStartupError { message },
        Event::IncompatibleAgent { .. } => View::IncompatibleAgent,
        Event::AgentSwitched { from, to, reason } => View::AgentSwitched { from, to, reason },
        Event::SessionCleared => View::SessionCleared,
        Event::ConversationCompacted => View::ConversationCompacted,
        Event::ConversationCompactionSummary {
            compaction_id,
            text,
        } => View::ConversationCompactionSummary {
            compaction_id,
            text,
        },
        Event::SessionContextReset { reason } => View::SessionContextReset { reason },
        Event::PromptRuntimeError { message } => View::PromptRuntimeError { message },
        Event::ModeSwitchFailed { mode_id, reason } => View::ModeSwitchFailed { mode_id, reason },
        Event::SessionNotice {
            severity,
            title,
            description,
            key,
        } => View::SessionNotice {
            severity,
            title,
            description: description.as_deref(),
            key: key.as_deref(),
        },
        Event::RateLimitAutoResumed { resets_at, manual } => View::RateLimitAutoResumed {
            resets_at: *resets_at,
            manual: *manual,
        },
        Event::ConversationSummary { text, .. } => View::ConversationSummary { text },
        Event::ThinkingStarted => View::ThinkingStarted,
        Event::PromptCapabilities { steering, .. } => View::PromptCapabilities {
            steering: *steering,
        },
        _ => View::Other,
    }
}
