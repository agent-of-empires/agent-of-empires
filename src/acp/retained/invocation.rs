use std::{io, rc::Rc};

use pulldown_cmark::disk::TextRef;

use super::{
    collections::{DiskRows, DiskSet},
    Context, Disk, Positions,
};
use crate::acp::transcript::{
    event::View,
    reducer::{Core, Projection},
    storage::{Changes, DeltaOf, RowOf, Rows, Set},
    TranscriptDelta, TranscriptRowKind as Kind,
};

/// Disk-backed replay provenance, captured before shared start-row replacement.
pub(crate) struct Replay {
    model: Core<Disk>,
    invocations: Invocations,
}
pub(crate) struct Invocations {
    context: Rc<Context>,
    pub rows: DiskRows,
    positions: Positions,
    current: Positions,
    terminal: DiskSet,
    completions: i64,
    removed: i64,
}
impl Replay {
    pub fn new(context: Rc<Context>) -> io::Result<Self> {
        Ok(Self {
            model: Core::new(Disk(Rc::clone(&context)))?,
            invocations: Invocations {
                rows: DiskRows::new(Rc::clone(&context))?,
                positions: Positions::new(Rc::clone(&context))?,
                current: Positions::new(Rc::clone(&context))?,
                terminal: DiskSet::new(Rc::clone(&context))?,
                completions: context.store.collection()?,
                removed: context.store.collection()?,
                context,
            },
        })
    }
    pub fn apply(&mut self, sequence: u64, event: View<'_, Disk>) -> io::Result<()> {
        let reopening = match &event {
            View::ToolCallStarted { tool_call }
                if self.invocations.terminal.contains(tool_call.id)? =>
            {
                Some(tool_call.id)
            }
            _ => None,
        };
        for delta in self.model.apply(sequence, event)?.into_iter() {
            self.invocations.apply(delta?, reopening)?;
        }
        Ok(())
    }
    pub fn into_projection(self) -> io::Result<(Projection<Disk>, Invocations)> {
        Ok((self.model.into_projection()?, self.invocations))
    }
}
impl Invocations {
    pub fn visible(&self, index: u64) -> io::Result<bool> {
        Ok(self
            .context
            .store
            .floor_record::<u64>(self.removed, index)?
            .is_none_or(|(position, _)| position != index))
    }
    pub fn completion(&self, start: u64) -> io::Result<Option<super::records::Owned<RowOf<Disk>>>> {
        match self
            .context
            .store
            .floor_record::<u64>(self.completions, start)?
        {
            Some((position, index)) if position == start && self.visible(index)? => {
                self.rows.read(index).map(Some)
            }
            _ => Ok(None),
        }
    }
    fn apply(&mut self, delta: DeltaOf<Disk>, reopening: Option<TextRef>) -> io::Result<()> {
        let (row, patch) = match delta {
            TranscriptDelta::Append(row) => (row, false),
            TranscriptDelta::Patch { row, .. } => (row, true),
            TranscriptDelta::Remove(id) => {
                for index in 0..self.rows.len() {
                    if self.context.equal(self.rows.read(index)?.id, id)? {
                        self.context.store.write(self.removed, index, &1u64)?;
                    }
                }
                return Ok(());
            }
        };
        let new_invocation = row.kind == Kind::ToolStart
            && row
                .tool_call_id
                .zip(reopening)
                .map(|(id, reopened)| self.context.equal(id, reopened))
                .transpose()?
                .unwrap_or(false);
        let previous = if patch && !new_invocation {
            self.positions.get(row.id)?
        } else {
            None
        };
        let position = match previous {
            Some(index) if self.visible(index)? => index,
            _ => self.rows.len(),
        };
        let id = row.id;
        let tool = row.tool_call_id;
        let kind = row.kind;
        if position == self.rows.len() {
            self.rows.push(&row)?;
        } else {
            self.rows.update(position, |stored| {
                *stored = row;
                Ok(())
            })?;
        }
        self.positions.insert(id, position)?;
        if let Some(tool) = tool {
            match kind {
                Kind::ToolStart => {
                    self.current.insert(tool, position)?;
                    if new_invocation {
                        self.terminal.remove(tool)?;
                    }
                }
                Kind::ToolComplete | Kind::ToolError | Kind::ToolStopped => {
                    self.terminal.insert(tool)?;
                    if let Some(start) = self.current.get(tool)? {
                        self.context
                            .store
                            .write(self.completions, start, &position)?;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
}
