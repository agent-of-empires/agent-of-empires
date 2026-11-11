use std::{
    cell::Cell,
    io::{self, Write},
    marker::PhantomData,
    rc::Rc,
};

use pulldown_cmark::disk::{Record, TextRef};
use sha2::{Digest, Sha256};

use super::records::{put, record, take, Byte, Owned, RowArena};
use super::{invalid, Context, Disk};
use crate::acp::transcript::{
    reducer::{Buffer, ToolLifecycle},
    storage::{Changes, DeltaOf, Outputs, RowOf, Rows, Set, Tools},
    TranscriptDelta, UnfinishedToolOutput,
};

pub(crate) struct DiskRows(pub(super) RowArena);
impl DiskRows {
    pub fn new(context: Rc<Context>) -> io::Result<Self> {
        Ok(Self(RowArena::new(context)?))
    }
}
impl Rows<Disk> for DiskRows {
    type Read<'a> = Owned<RowOf<Disk>>;
    fn len(&self) -> u64 {
        self.0.length
    }
    fn read(&self, index: u64) -> io::Result<Self::Read<'_>> {
        self.0.read(index).map(Owned)
    }
    fn update<R>(
        &mut self,
        index: u64,
        operation: impl FnOnce(&mut RowOf<Disk>) -> io::Result<R>,
    ) -> io::Result<R> {
        let mut row = self.0.read(index)?;
        let value = operation(&mut row)?;
        self.0.write(index, &row)?;
        Ok(value)
    }
    fn push(&mut self, row: &RowOf<Disk>) -> io::Result<()> {
        self.0.push(row).map(|_| ())
    }
    fn retain(&mut self, mut keep: impl FnMut(&RowOf<Disk>) -> io::Result<bool>) -> io::Result<()> {
        let mut destination = 0;
        for source in 0..self.0.length {
            let row = self.0.read(source)?;
            if keep(&row)? {
                if source != destination {
                    self.0.write(destination, &row)?;
                }
                destination += 1;
            }
        }
        if destination != self.0.length {
            self.0.truncate(destination)?;
        }
        Ok(())
    }
}

trait Keyed: Record {
    fn key(&self) -> TextRef;
}
impl Keyed for TextRef {
    fn key(&self) -> TextRef {
        *self
    }
}
struct DigestWriter(Sha256);
impl Write for DigestWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
struct KeyMap<T> {
    context: Rc<Context>,
    collection: i64,
    digest: Cell<Option<(TextRef, [u8; 32])>>,
    marker: PhantomData<T>,
}
impl<T: Keyed> KeyMap<T> {
    fn new(context: Rc<Context>) -> io::Result<Self> {
        let collection = context.store.collection()?;
        Ok(Self {
            context,
            collection,
            digest: Cell::new(None),
            marker: PhantomData,
        })
    }
    fn digest(&self, text: TextRef) -> io::Result<[u8; 32]> {
        self.context.spool.view(text)?;
        if let Some((cached, digest)) = self.digest.get() {
            if cached == text {
                return Ok(digest);
            }
        }
        let mut digest = DigestWriter(Sha256::new());
        self.context.spool.copy_to(text, &mut digest)?;
        let digest = digest.0.finalize().into();
        self.digest.set(Some((text, digest)));
        Ok(digest)
    }
    fn get(&self, text: TextRef) -> io::Result<Option<(u64, T)>> {
        let digest = self.digest(text)?;
        let mut after = 0;
        while let Some((cursor, value)) =
            self.context
                .store
                .lookup::<T>(self.collection, &digest, after)?
        {
            if self.context.equal(value.key(), text)? {
                return Ok(Some((cursor, value)));
            }
            after = cursor;
        }
        Ok(None)
    }
    fn cursor(&self, cursor: u64) -> io::Result<T> {
        let after = cursor
            .checked_sub(1)
            .ok_or_else(|| invalid("invalid retained map cursor"))?;
        match self.context.store.next_entry(self.collection, after)? {
            Some((found, value)) if found == cursor => Ok(value),
            _ => Err(invalid("retained map cursor disappeared")),
        }
    }
    fn insert(&mut self, value: &T) -> io::Result<()> {
        let key = value.key();
        if let Some((cursor, _)) = self.get(key)? {
            self.context.store.replace(cursor, value)
        } else {
            self.context
                .store
                .insert(self.collection, &self.digest(key)?, value)
                .map(|_| ())
        }
    }
    fn next(&self, after: u64) -> io::Result<Option<(u64, T)>> {
        self.context.store.next_entry(self.collection, after)
    }
}
impl<T> Drop for KeyMap<T> {
    fn drop(&mut self) {
        let _ = self.context.store.clear_entries(self.collection);
    }
}
struct KeyIter<T> {
    map: KeyMap<T>,
    after: u64,
    done: bool,
}
impl<T: Keyed> Iterator for KeyIter<T> {
    type Item = io::Result<T>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        match self.map.next(self.after) {
            Ok(Some((cursor, value))) => {
                self.after = cursor;
                Some(Ok(value))
            }
            Ok(None) => {
                self.done = true;
                None
            }
            Err(error) => {
                self.done = true;
                Some(Err(error))
            }
        }
    }
}

struct Position {
    id: TextRef,
    index: u64,
}
record!(Position {
    id: TextRef,
    index: u64
});
impl Keyed for Position {
    fn key(&self) -> TextRef {
        self.id
    }
}
pub(crate) struct Positions(KeyMap<Position>);
impl Positions {
    pub fn new(context: Rc<Context>) -> io::Result<Self> {
        KeyMap::new(context).map(Self)
    }
    pub fn get(&self, id: TextRef) -> io::Result<Option<u64>> {
        Ok(self.0.get(id)?.map(|(_, value)| value.index))
    }
    pub fn insert(&mut self, id: TextRef, index: u64) -> io::Result<()> {
        self.0.insert(&Position { id, index })
    }
}
impl crate::acp::transcript::storage::Positions<Disk> for Positions {
    fn get<'a>(&self, id: TextRef) -> io::Result<Option<u64>> {
        Positions::get(self, id)
    }
    fn insert(&mut self, id: TextRef, index: u64) -> io::Result<()> {
        Positions::insert(self, id, index)
    }
    fn remove<'a>(&mut self, id: TextRef) -> io::Result<()> {
        match self.0.get(id)? {
            Some((cursor, _)) => self.0.context.store.remove(cursor).map(|_| ()),
            None => Ok(()),
        }
    }
    fn clear(&mut self) -> io::Result<()> {
        self.0.context.store.clear_entries(self.0.collection)
    }
}

pub(crate) struct DiskSet(KeyMap<TextRef>);
impl DiskSet {
    pub fn new(context: Rc<Context>) -> io::Result<Self> {
        KeyMap::new(context).map(Self)
    }
}
impl Set<Disk> for DiskSet {
    fn contains<'a>(&self, value: TextRef) -> io::Result<bool> {
        Ok(self.0.get(value)?.is_some())
    }
    fn insert(&mut self, value: TextRef) -> io::Result<()> {
        self.0.insert(&value)
    }
    fn remove<'a>(&mut self, value: TextRef) -> io::Result<bool> {
        match self.0.get(value)? {
            Some((cursor, _)) => self.0.context.store.remove(cursor),
            None => Ok(false),
        }
    }
    fn clear(&mut self) -> io::Result<()> {
        self.0.context.store.clear_entries(self.0.collection)
    }
}

struct Life {
    id: TextRef,
    kind: Byte,
    first_seq: Option<u64>,
    text: TextRef,
}
record!(Life { id: TextRef, kind: Byte, first_seq: Option<u64>, text: TextRef });
impl Keyed for Life {
    fn key(&self) -> TextRef {
        self.id
    }
}
impl Life {
    fn from(id: TextRef, lifecycle: ToolLifecycle<TextRef>) -> Self {
        let (kind, buffer) = match lifecycle {
            ToolLifecycle::Buffered(buffer) => (0, buffer),
            ToolLifecycle::Open(buffer) => (1, buffer),
            ToolLifecycle::Terminal(buffer) => (2, buffer),
        };
        Self {
            id,
            kind: Byte(kind),
            first_seq: buffer.first_seq,
            text: buffer.text,
        }
    }
    fn state(self) -> io::Result<ToolLifecycle<TextRef>> {
        let buffer = Buffer {
            first_seq: self.first_seq,
            text: self.text,
        };
        Ok(match self.kind.0 {
            0 => ToolLifecycle::Buffered(buffer),
            1 => ToolLifecycle::Open(buffer),
            2 => ToolLifecycle::Terminal(buffer),
            _ => return Err(invalid("invalid retained tool lifecycle")),
        })
    }
}
pub(crate) struct DiskTools(KeyMap<Life>);
impl DiskTools {
    pub fn new(context: Rc<Context>) -> io::Result<Self> {
        KeyMap::new(context).map(Self)
    }
}
impl Tools<Disk> for DiskTools {
    type Read<'a> = Owned<ToolLifecycle<TextRef>>;
    type Key<'a> = Owned<TextRef>;
    fn get<'a>(&self, value: TextRef) -> io::Result<Option<(u64, Self::Read<'_>)>> {
        self.0
            .get(value)?
            .map(|(cursor, life)| Ok((cursor, Owned(life.state()?))))
            .transpose()
    }
    fn next(&self, after: Option<u64>) -> io::Result<Option<(u64, Self::Key<'_>, Self::Read<'_>)>> {
        self.0
            .next(after.unwrap_or(0))?
            .map(|(cursor, life)| Ok((cursor, Owned(life.id), Owned(life.state()?))))
            .transpose()
    }
    fn update<R>(
        &mut self,
        cursor: u64,
        operation: impl FnOnce(&mut ToolLifecycle<TextRef>) -> io::Result<R>,
    ) -> io::Result<R> {
        let life = self.0.cursor(cursor)?;
        let id = life.id;
        let mut state = life.state()?;
        let result = operation(&mut state)?;
        self.0
            .context
            .store
            .replace(cursor, &Life::from(id, state))?;
        Ok(result)
    }
    fn insert(&mut self, id: TextRef, state: ToolLifecycle<TextRef>) -> io::Result<()> {
        self.0.insert(&Life::from(id, state))
    }
    fn promote(&mut self, cursor: u64) -> io::Result<()> {
        let life = self.0.cursor(cursor)?;
        let digest = self.0.digest(life.id)?;
        self.0.context.store.remove(cursor)?;
        self.0
            .context
            .store
            .insert(self.0.collection, &digest, &life)?;
        Ok(())
    }
    fn into_iter(self) -> impl Iterator<Item = io::Result<(TextRef, ToolLifecycle<TextRef>)>> {
        KeyIter {
            map: self.0,
            after: 0,
            done: false,
        }
        .map(|value| {
            let value = value?;
            Ok((value.id, value.state()?))
        })
    }
}

struct Output {
    id: TextRef,
    text: TextRef,
    first_seq: u64,
}
record!(Output {
    id: TextRef,
    text: TextRef,
    first_seq: u64
});
pub(crate) struct DiskOutputs {
    context: Rc<Context>,
    collection: i64,
}
impl DiskOutputs {
    pub fn new(context: Rc<Context>) -> io::Result<Self> {
        let collection = context.store.collection()?;
        Ok(Self {
            context,
            collection,
        })
    }
    pub fn iter(&self) -> impl Iterator<Item = io::Result<UnfinishedToolOutput<TextRef>>> + '_ {
        let mut after = None;
        let mut done = false;
        std::iter::from_fn(move || {
            if done {
                return None;
            }
            match self
                .context
                .store
                .next_record::<Output>(self.collection, after)
            {
                Ok(Some((position, value))) => {
                    after = Some(position);
                    Some(Ok(UnfinishedToolOutput {
                        tool_call_id: value.id,
                        text: value.text,
                        first_seq: value.first_seq,
                    }))
                }
                Ok(None) => {
                    done = true;
                    None
                }
                Err(error) => {
                    done = true;
                    Some(Err(error))
                }
            }
        })
    }
}
impl Outputs<Disk> for DiskOutputs {
    fn push(&mut self, value: UnfinishedToolOutput<TextRef>) -> io::Result<()> {
        if self
            .context
            .store
            .next_record::<Output>(self.collection, value.first_seq.checked_sub(1))?
            .is_some_and(|(seq, _)| seq == value.first_seq)
        {
            return Err(invalid("duplicate retained output sequence"));
        }
        self.context.store.write(
            self.collection,
            value.first_seq,
            &Output {
                id: value.tool_call_id,
                text: value.text,
                first_seq: value.first_seq,
            },
        )
    }
    fn retain(
        &mut self,
        mut keep: impl FnMut(&UnfinishedToolOutput<TextRef>) -> io::Result<bool>,
    ) -> io::Result<()> {
        let mut after = None;
        while let Some((position, value)) = self
            .context
            .store
            .next_record::<Output>(self.collection, after)?
        {
            after = Some(position);
            let output = UnfinishedToolOutput {
                tool_call_id: value.id,
                text: value.text,
                first_seq: value.first_seq,
            };
            if !keep(&output)? {
                self.context
                    .store
                    .remove_record(self.collection, position)?;
            }
        }
        Ok(())
    }
}
impl Drop for DiskOutputs {
    fn drop(&mut self) {
        let _ = self.context.store.truncate(self.collection, 0);
    }
}

struct Delta {
    kind: Byte,
    row: u64,
    id: TextRef,
}
record!(Delta {
    kind: Byte,
    row: u64,
    id: TextRef
});
struct ChangeRows {
    arena: RowArena,
    collection: i64,
}
impl Drop for ChangeRows {
    fn drop(&mut self) {
        let _ = self.arena.context.store.truncate(self.collection, 0);
    }
}
pub(crate) struct DiskChanges {
    context: Rc<Context>,
    data: Option<ChangeRows>,
    length: u64,
}
impl DiskChanges {
    pub fn new(context: Rc<Context>) -> Self {
        Self {
            context,
            data: None,
            length: 0,
        }
    }
}
impl Changes<Disk> for DiskChanges {
    fn is_empty(&self) -> bool {
        self.length == 0
    }
    fn push(&mut self, delta: DeltaOf<Disk>) -> io::Result<()> {
        if self.data.is_none() {
            self.data = Some(ChangeRows {
                arena: RowArena::new(Rc::clone(&self.context))?,
                collection: self.context.store.collection()?,
            });
        }
        let data = self.data.as_mut().expect("change rows initialized");
        let (kind, row, id) = match delta {
            TranscriptDelta::Append(row) => (0, data.arena.push(&row)?, row.id),
            TranscriptDelta::Patch { id, row } => (1, data.arena.push(&row)?, id),
            TranscriptDelta::Remove(id) => (2, 0, id),
        };
        let next = self
            .length
            .checked_add(1)
            .ok_or_else(|| invalid("retained delta count overflow"))?;
        self.context.store.write(
            data.collection,
            self.length,
            &Delta {
                kind: Byte(kind),
                row,
                id,
            },
        )?;
        self.length = next;
        Ok(())
    }
    fn into_iter(self) -> impl Iterator<Item = io::Result<DeltaOf<Disk>>> {
        let mut index = 0;
        let mut done = false;
        std::iter::from_fn(move || {
            if done || index == self.length {
                return None;
            }
            let result = (|| {
                let data = self
                    .data
                    .as_ref()
                    .ok_or_else(|| invalid("missing retained delta arena"))?;
                let delta: Delta = self.context.store.read(data.collection, index)?;
                Ok(match delta.kind.0 {
                    0 => TranscriptDelta::Append(data.arena.read(delta.row)?),
                    1 => TranscriptDelta::Patch {
                        id: delta.id,
                        row: data.arena.read(delta.row)?,
                    },
                    2 => TranscriptDelta::Remove(delta.id),
                    _ => return Err(invalid("invalid retained delta kind")),
                })
            })();
            index += 1;
            if result.is_err() {
                done = true;
            }
            Some(result)
        })
    }
}
