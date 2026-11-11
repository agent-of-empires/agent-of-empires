use std::{io, ops::Deref, rc::Rc};

use chrono::{DateTime, Utc};
use pulldown_cmark::disk::{Record, TextRef};

use super::json::ValueRef;
use super::{invalid, Context, Disk, List};
use crate::acp::{
    state::ToolCall,
    transcript::{storage::RowOf, TranscriptRowKind},
};

pub(super) fn put<T: Record>(value: &T, bytes: &mut [u8], position: &mut usize) {
    value.encode(&mut bytes[*position..*position + T::SIZE]);
    *position += T::SIZE;
}
pub(super) fn take<T: Record>(bytes: &[u8], position: &mut usize) -> io::Result<T> {
    let value = T::decode(&bytes[*position..*position + T::SIZE])?;
    *position += T::SIZE;
    Ok(value)
}

macro_rules! record {
    ($name:ident { $($field:ident: $ty:ty),* $(,)? }) => {
        impl Record for $name {
            const SIZE: usize = 0 $(+ <$ty as Record>::SIZE)*;
            fn encode(&self, bytes: &mut [u8]) {
                let mut position = 0;
                $(put(&self.$field, bytes, &mut position);)*
                debug_assert_eq!(position, Self::SIZE);
            }
            fn decode(bytes: &[u8]) -> io::Result<Self> {
                if bytes.len() != Self::SIZE { return Err(invalid("invalid retained record size")) }
                let mut position = 0;
                let value = Self { $($field: take::<$ty>(bytes, &mut position)?),* };
                debug_assert_eq!(position, Self::SIZE);
                Ok(value)
            }
        }
    };
}
pub(super) use record;

#[derive(Clone, Copy)]
pub(super) struct Byte(pub u8);
impl Record for Byte {
    const SIZE: usize = 1;
    fn encode(&self, bytes: &mut [u8]) {
        bytes[0] = self.0;
    }
    fn decode(bytes: &[u8]) -> io::Result<Self> {
        match bytes {
            [value] => Ok(Self(*value)),
            _ => Err(invalid("invalid retained byte")),
        }
    }
}

#[derive(Clone, Copy)]
struct Stamp(DateTime<Utc>);
impl Record for Stamp {
    const SIZE: usize = 12;
    fn encode(&self, bytes: &mut [u8]) {
        bytes[..8].copy_from_slice(&self.0.timestamp().to_le_bytes());
        bytes[8..].copy_from_slice(&self.0.timestamp_subsec_nanos().to_le_bytes());
    }
    fn decode(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() != Self::SIZE {
            return Err(invalid("invalid retained timestamp size"));
        }
        let seconds = i64::from_le_bytes(bytes[..8].try_into().map_err(io::Error::other)?);
        let nanos = u32::from_le_bytes(bytes[8..].try_into().map_err(io::Error::other)?);
        DateTime::from_timestamp(seconds, nanos)
            .map(Self)
            .ok_or_else(|| invalid("invalid retained timestamp"))
    }
}

impl Record for List {
    const SIZE: usize = <Option<ValueRef> as Record>::SIZE + 8;
    fn encode(&self, bytes: &mut [u8]) {
        let mut position = 0;
        put(&self.value, bytes, &mut position);
        put(&self.length, bytes, &mut position);
    }
    fn decode(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() != Self::SIZE {
            return Err(invalid("invalid retained list size"));
        }
        let mut position = 0;
        let value = Self {
            value: take(bytes, &mut position)?,
            length: take(bytes, &mut position)?,
        };
        if value.value.is_none() && value.length != 0 {
            return Err(invalid("invalid empty retained list"));
        }
        Ok(value)
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Comments {
    pub intro: TextRef,
    pub outro: TextRef,
    pub is_multi_repo: bool,
    pub comments: List,
}
#[derive(Clone, Copy)]
struct CommentRecord {
    intro: TextRef,
    outro: TextRef,
    multi: Byte,
    comments: List,
}
record!(CommentRecord {
    intro: TextRef,
    outro: TextRef,
    multi: Byte,
    comments: List
});

#[derive(Clone, Copy)]
struct Head {
    id: TextRef,
    group: TextRef,
    text: TextRef,
    tool_id: Option<TextRef>,
    at: Stamp,
    kind: Byte,
    flags: Byte,
}
record!(Head { id: TextRef, group: TextRef, text: TextRef, tool_id: Option<TextRef>, at: Stamp, kind: Byte, flags: Byte });
struct Detail {
    output: List,
    attachments: List,
    origin: Option<TextRef>,
    answers: List,
    prompt_ids: List,
}
record!(Detail { output: List, attachments: List, origin: Option<TextRef>, answers: List, prompt_ids: List });
struct ToolHead {
    id: TextRef,
    name: TextRef,
    kind: TextRef,
    args: TextRef,
    at: Stamp,
}
record!(ToolHead {
    id: TextRef,
    name: TextRef,
    kind: TextRef,
    args: TextRef,
    at: Stamp
});
struct ToolExtra {
    parent: Option<TextRef>,
    recall: Option<ValueRef>,
    diffs: List,
}
record!(ToolExtra { parent: Option<TextRef>, recall: Option<ValueRef>, diffs: List });

pub(crate) struct Owned<T>(pub T);
impl<T> Deref for Owned<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

#[cfg(test)]
thread_local! {
    pub(super) static ROW_READS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

pub(super) struct RowArena {
    pub context: Rc<Context>,
    collections: [i64; 5],
    pub length: u64,
}
impl RowArena {
    pub fn new(context: Rc<Context>) -> io::Result<Self> {
        let store = &context.store;
        let collections = [
            store.collection()?,
            store.collection()?,
            store.collection()?,
            store.collection()?,
            store.collection()?,
        ];
        Ok(Self {
            context,
            collections,
            length: 0,
        })
    }
    pub fn read(&self, index: u64) -> io::Result<RowOf<Disk>> {
        #[cfg(test)]
        ROW_READS.with(|reads| reads.set(reads.get() + 1));
        if index >= self.length {
            return Err(invalid("unknown retained row"));
        }
        let store = &self.context.store;
        let head: Head = store.read(self.collections[0], index)?;
        let detail: Detail = store.read(self.collections[1], index)?;
        if head.flags.0 & !15 != 0 {
            return Err(invalid("invalid retained row flags"));
        }
        let tool = if head.flags.0 & 1 != 0 {
            let tool: ToolHead = store.read(self.collections[2], index)?;
            let extra: ToolExtra = store.read(self.collections[3], index)?;
            Some(ToolCall {
                id: tool.id,
                name: tool.name,
                kind: tool.kind,
                args_preview: tool.args,
                started_at: tool.at.0,
                parent_tool_call_id: extra.parent,
                memory_recall: extra.recall,
                diffs: extra.diffs,
            })
        } else {
            None
        };
        let diff_comments = if head.flags.0 & 2 != 0 {
            let comments: CommentRecord = store.read(self.collections[4], index)?;
            let is_multi_repo = match comments.multi.0 {
                0 => false,
                1 => true,
                _ => return Err(invalid("invalid retained comment flag")),
            };
            Some(Comments {
                intro: comments.intro,
                outro: comments.outro,
                is_multi_repo,
                comments: comments.comments,
            })
        } else {
            None
        };
        Ok(crate::acp::transcript::TranscriptRow {
            id: head.id,
            group_id: head.group,
            kind: decode_kind(head.kind.0)?,
            at: head.at.0,
            text: head.text,
            tool_call_id: head.tool_id,
            tool,
            output: detail.output,
            attachments: detail.attachments,
            origin_plugin: detail.origin,
            merged_prompt_ids: detail.prompt_ids,
            diff_comments,
            elicitation_answers: detail.answers,
            async_subagent: head.flags.0 & 4 != 0,
            message_start: head.flags.0 & 8 != 0,
        })
    }
    pub fn write(&self, index: u64, row: &RowOf<Disk>) -> io::Result<()> {
        let store = &self.context.store;
        let flags = u8::from(row.tool.is_some())
            | (u8::from(row.diff_comments.is_some()) << 1)
            | (u8::from(row.async_subagent) << 2)
            | (u8::from(row.message_start) << 3);
        store.write(
            self.collections[0],
            index,
            &Head {
                id: row.id,
                group: row.group_id,
                text: row.text,
                tool_id: row.tool_call_id,
                at: Stamp(row.at),
                kind: Byte(encode_kind(row.kind)),
                flags: Byte(flags),
            },
        )?;
        store.write(
            self.collections[1],
            index,
            &Detail {
                output: row.output,
                attachments: row.attachments,
                origin: row.origin_plugin,
                answers: row.elicitation_answers,
                prompt_ids: row.merged_prompt_ids,
            },
        )?;
        if let Some(tool) = &row.tool {
            store.write(
                self.collections[2],
                index,
                &ToolHead {
                    id: tool.id,
                    name: tool.name,
                    kind: tool.kind,
                    args: tool.args_preview,
                    at: Stamp(tool.started_at),
                },
            )?;
            store.write(
                self.collections[3],
                index,
                &ToolExtra {
                    parent: tool.parent_tool_call_id,
                    recall: tool.memory_recall,
                    diffs: tool.diffs,
                },
            )?;
        }
        if let Some(comments) = &row.diff_comments {
            store.write(
                self.collections[4],
                index,
                &CommentRecord {
                    intro: comments.intro,
                    outro: comments.outro,
                    multi: Byte(u8::from(comments.is_multi_repo)),
                    comments: comments.comments,
                },
            )?;
        }
        Ok(())
    }
    pub fn push(&mut self, row: &RowOf<Disk>) -> io::Result<u64> {
        let index = self.length;
        let length = index
            .checked_add(1)
            .ok_or_else(|| invalid("retained row count overflow"))?;
        self.write(index, row)?;
        self.length = length;
        Ok(index)
    }
    pub fn truncate(&mut self, length: u64) -> io::Result<()> {
        for collection in self.collections {
            self.context.store.truncate(collection, length)?;
        }
        self.length = length;
        Ok(())
    }
}
impl Drop for RowArena {
    fn drop(&mut self) {
        for collection in self.collections {
            let _ = self.context.store.truncate(collection, 0);
        }
    }
}

fn encode_kind(kind: TranscriptRowKind) -> u8 {
    use TranscriptRowKind::*;
    match kind {
        ToolStart => 0,
        ToolComplete => 1,
        ToolError => 2,
        ToolStopped => 3,
        Message => 4,
        Thinking => 5,
        UserPrompt => 6,
        UserDiffComments => 7,
        ElicitationAnswered => 8,
        EmptyOutput => 9,
        ContextReset => 10,
        SessionCleared => 11,
        Compacted => 12,
        CompactionSummary => 13,
        Summary => 14,
        Notice => 15,
        Advisory => 16,
    }
}
fn decode_kind(kind: u8) -> io::Result<TranscriptRowKind> {
    use TranscriptRowKind::*;
    Ok(match kind {
        0 => ToolStart,
        1 => ToolComplete,
        2 => ToolError,
        3 => ToolStopped,
        4 => Message,
        5 => Thinking,
        6 => UserPrompt,
        7 => UserDiffComments,
        8 => ElicitationAnswered,
        9 => EmptyOutput,
        10 => ContextReset,
        11 => SessionCleared,
        12 => Compacted,
        13 => CompactionSummary,
        14 => Summary,
        15 => Notice,
        16 => Advisory,
        _ => return Err(invalid("invalid retained row kind")),
    })
}
