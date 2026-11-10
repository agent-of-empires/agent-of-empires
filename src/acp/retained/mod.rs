use std::{
    io::{self, Write},
    rc::Rc,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
};

use pulldown_cmark::disk::{ByteSource, Cursor, Spool, Store, TextRef};

use crate::acp::{
    state::PayloadCollection,
    transcript::storage::{Backend, TextWriter},
};

mod collections;
mod date;
mod event;
pub(crate) mod invocation;
mod json;
pub(crate) mod load;
mod records;

pub(crate) use collections::Positions;
use collections::{DiskChanges, DiskOutputs, DiskRows, DiskSet, DiskTools};
use json::{Arena, Kind, ValueRef};
use records::Comments;

static NEXT_SOURCE: AtomicU64 = AtomicU64::new(1);

pub(crate) struct Context {
    pub store: Store,
    pub spool: Spool,
    arena: Arena,
    empty: TextRef,
}

impl Context {
    pub fn new(cancelled: Arc<AtomicBool>) -> io::Result<Rc<Self>> {
        let mut source = NEXT_SOURCE.load(Ordering::Relaxed);
        loop {
            let next = source
                .checked_add(1)
                .ok_or_else(|| invalid("retained source identifier overflow"))?;
            match NEXT_SOURCE.compare_exchange_weak(
                source,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(current) => source = current,
            }
        }
        let store = Store::new(Arc::clone(&cancelled))?;
        let spool = Spool::new(source, cancelled)?;
        let empty = spool.append(b"")?;
        let arena = Arena::new(&store)?;
        Ok(Rc::new(Self {
            store,
            spool,
            arena,
            empty,
        }))
    }

    pub fn parse(&self, input: TextRef) -> io::Result<ValueRef> {
        self.arena.parse(&self.store, &self.spool, input)
    }
    pub fn text(&self, value: ValueRef) -> io::Result<TextRef> {
        self.arena.text(&self.store, value)
    }
    pub fn property(&self, value: ValueRef, name: &str) -> io::Result<Option<ValueRef>> {
        self.arena.property(&self.store, &self.spool, value, name)
    }
    pub fn string(&self, value: ValueRef, name: &str) -> io::Result<TextRef> {
        self.text(
            self.property(value, name)?
                .ok_or_else(|| invalid("retained string field missing"))?,
        )
    }
    pub fn optional_text(&self, value: ValueRef, name: &str) -> io::Result<Option<TextRef>> {
        let Some(value) = self.property(value, name)? else {
            return Ok(None);
        };
        if self.arena.node(&self.store, value)?.kind == Kind::Null {
            return Ok(None);
        }
        self.text(value).map(Some)
    }
    pub fn text_is(&self, value: TextRef, expected: &str) -> io::Result<bool> {
        json::text_is(&self.spool, value, expected)
    }
    pub fn items(&self, list: List) -> io::Result<impl Iterator<Item = io::Result<ValueRef>> + '_> {
        Ok(self
            .check_list(list)?
            .value
            .map(|value| self.arena.children(&self.store, value))
            .transpose()?
            .into_iter()
            .flatten())
    }
    pub fn list_length(&self, list: List) -> io::Result<u64> {
        Ok(self.check_list(list)?.length)
    }
    fn list(&self, value: ValueRef) -> io::Result<List> {
        let node = self.arena.node(&self.store, value)?;
        if node.kind != Kind::Array {
            return Err(invalid("JSON array expected"));
        }
        Ok(List {
            value: Some(value),
            length: node.count,
        })
    }
    fn check_list(&self, value: List) -> io::Result<List> {
        match value.value {
            Some(reference) if self.list(reference)? == value => Ok(value),
            None if value.length == 0 => Ok(value),
            _ => Err(invalid("invalid retained collection")),
        }
    }
    fn equal(&self, left: TextRef, right: TextRef) -> io::Result<bool> {
        let left_source = self.spool.view(left)?;
        let right_source = self.spool.view(right)?;
        if left.len != right.len {
            return Ok(false);
        }
        if left == right {
            return Ok(true);
        }
        let mut a = [0; 8192];
        let mut b = [0; 8192];
        let mut offset = 0;
        while offset < left.len {
            let count = usize::try_from((left.len - offset).min(a.len() as u64))
                .map_err(io::Error::other)?;
            if left_source.read_at(offset, &mut a[..count])? != count
                || right_source.read_at(offset, &mut b[..count])? != count
            {
                return Err(invalid("retained text disappeared"));
            }
            if a[..count] != b[..count] {
                return Ok(false);
            }
            offset += count as u64;
        }
        Ok(true)
    }
    fn trim(&self, value: TextRef) -> io::Result<TextRef> {
        let source = self.spool.view(value)?;
        let mut cursor = Cursor::new(&source, 0..value.len)?;
        let start = loop {
            let start = cursor.position();
            match cursor.next_char()? {
                Some(ch) if ch.is_whitespace() => {}
                Some(_) => break start,
                None => return value.subrange(value.len..value.len),
            }
        };
        cursor.seek(value.len)?;
        while let Some(ch) = cursor.previous_char()? {
            if !ch.is_whitespace() {
                return value.subrange(start..cursor.position() + ch.len_utf8() as u64);
            }
        }
        Err(invalid("retained trim boundary disappeared"))
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct List {
    value: Option<ValueRef>,
    length: u64,
}
impl PayloadCollection for List {
    fn is_empty(&self) -> bool {
        self.length == 0
    }
}

#[derive(Clone)]
pub(crate) struct Disk(pub Rc<Context>);

pub(crate) struct Writer<'a> {
    spool: &'a Spool,
    writer: pulldown_cmark::disk::Writer<'a>,
}
impl TextWriter<Disk> for Writer<'_> {
    fn literal(&mut self, value: &str) -> io::Result<()> {
        self.writer.write_all(value.as_bytes())
    }
    fn text<'a>(&mut self, value: TextRef) -> io::Result<()> {
        self.spool.copy_to(value, &mut self.writer).map(|_| ())
    }
    fn number(&mut self, value: u64) -> io::Result<()> {
        write!(&mut self.writer, "{value}")
    }
    fn finish(self) -> io::Result<TextRef> {
        self.writer.finish()
    }
}

impl Backend for Disk {
    type Error = io::Error;
    type Text = TextRef;
    type Input<'a> = TextRef;
    type Recall = ValueRef;
    type RecallInput<'a> = ValueRef;
    type Diffs = List;
    type DiffsInput<'a> = List;
    type Output = List;
    type OutputInput<'a> = List;
    type Attachments = List;
    type AttachmentsInput<'a> = List;
    type Comments = Comments;
    type CommentsInput<'a> = List;
    type Answers = List;
    type AnswersInput<'a> = List;
    type Writer<'a> = Writer<'a>;
    type Rows = DiskRows;
    type Set = DiskSet;
    type Tools = DiskTools;
    type Outputs = DiskOutputs;
    type Changes = DiskChanges;

    fn rows(&self) -> io::Result<DiskRows> {
        DiskRows::new(Rc::clone(&self.0))
    }
    fn set(&self) -> io::Result<DiskSet> {
        DiskSet::new(Rc::clone(&self.0))
    }
    fn tools(&self) -> io::Result<DiskTools> {
        DiskTools::new(Rc::clone(&self.0))
    }
    fn outputs(&self) -> io::Result<DiskOutputs> {
        DiskOutputs::new(Rc::clone(&self.0))
    }
    fn changes(&self) -> DiskChanges {
        DiskChanges::new(Rc::clone(&self.0))
    }
    fn empty_text(&self) -> TextRef {
        self.0.empty
    }
    fn text_len(&self, value: &TextRef) -> u64 {
        value.len
    }
    fn input_len<'a>(&self, value: TextRef) -> u64 {
        value.len
    }
    fn borrow(&self, value: &TextRef) -> TextRef {
        *value
    }
    fn retain<'a>(&self, value: TextRef) -> io::Result<TextRef> {
        self.0.spool.view(value)?;
        Ok(value)
    }
    fn replace<'a>(&self, target: &mut TextRef, value: TextRef) -> io::Result<()> {
        *target = self.retain(value)?;
        Ok(())
    }
    fn equal<'a>(&self, left: &TextRef, right: TextRef) -> io::Result<bool> {
        self.0.equal(*left, right)
    }
    fn literal_equal<'a>(&self, value: TextRef, literal: &str) -> io::Result<bool> {
        json::text_is(&self.0.spool, value, literal)
    }
    fn trim_empty<'a>(&self, value: TextRef) -> io::Result<bool> {
        let source = self.0.spool.view(value)?;
        let mut cursor = Cursor::new(&source, 0..value.len)?;
        while let Some(ch) = cursor.next_char()? {
            if !ch.is_whitespace() {
                return Ok(false);
            }
        }
        Ok(true)
    }
    fn trim<'a>(&self, value: Self::Input<'a>) -> io::Result<Self::Input<'a>> {
        self.0.trim(value)
    }
    fn starts_with<'a>(&self, value: TextRef, prefix: &str) -> io::Result<bool> {
        if value.len < prefix.len() as u64 {
            return Ok(false);
        }
        json::text_is(
            &self.0.spool,
            value.subrange(0..prefix.len() as u64)?,
            prefix,
        )
    }
    fn writer(&self, _: u64) -> Writer<'_> {
        Writer {
            spool: &self.0.spool,
            writer: self.0.spool.writer(),
        }
    }
    fn borrow_recall(&self, value: &ValueRef) -> ValueRef {
        *value
    }
    fn borrow_diffs(&self, value: &List) -> List {
        *value
    }
    fn retain_recall<'a>(&self, value: ValueRef) -> io::Result<ValueRef> {
        self.0.arena.node(&self.0.store, value)?;
        Ok(value)
    }
    fn retain_diffs<'a>(&self, value: List) -> io::Result<List> {
        self.0.check_list(value)
    }
    fn retain_output<'a>(&self, value: List) -> io::Result<List> {
        self.0.check_list(value)
    }
    fn retain_attachments<'a>(&self, value: List) -> io::Result<List> {
        self.0.check_list(value)
    }
    fn retain_answers<'a>(&self, value: List) -> io::Result<List> {
        self.0.check_list(value)
    }
    fn comments<'a>(
        &self,
        intro: TextRef,
        outro: TextRef,
        is_multi_repo: bool,
        comments: List,
    ) -> io::Result<Comments> {
        Ok(Comments {
            intro: self.retain(intro)?,
            outro: self.retain(outro)?,
            is_multi_repo,
            comments: self.0.check_list(comments)?,
        })
    }
    fn answer_capacity<'a>(&self, _: List) -> u64 {
        0
    }
    fn answer_pairs<'a>(
        &self,
        value: Self::AnswersInput<'a>,
    ) -> impl Iterator<Item = io::Result<(Self::Input<'a>, Self::Input<'a>)>> {
        let children = self.0.check_list(value).and_then(|value| {
            value
                .value
                .map(|value| self.0.arena.children(&self.0.store, value))
                .transpose()
        });
        AnswerPairs {
            context: &self.0,
            children: Some(children),
        }
    }
}

struct AnswerPairs<'a> {
    context: &'a Context,
    children: Option<io::Result<Option<json::Children<'a>>>>,
}
impl Iterator for AnswerPairs<'_> {
    type Item = io::Result<(TextRef, TextRef)>;
    fn next(&mut self) -> Option<Self::Item> {
        let children = match self.children.as_mut()? {
            Ok(Some(children)) => children,
            Ok(None) => return None,
            Err(_) => {
                return self
                    .children
                    .take()
                    .map(|result| Err(result.err().expect("answer iterator error")))
            }
        };
        children.next().map(|value| {
            let value = value?;
            let question = self
                .context
                .property(value, "question")?
                .ok_or_else(|| invalid("missing elicitation question"))?;
            let answer = self
                .context
                .property(value, "answer")?
                .ok_or_else(|| invalid("missing elicitation answer"))?;
            Ok((self.context.text(question)?, self.context.text(answer)?))
        })
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests;
