use std::{cell::RefCell, io, ops::Range};

use pulldown_cmark::disk::{ByteSource, Record, Spool, Store, TextRef};
use ratatui::style::Style;

use super::style::StoredStyle;
use super::{ListStack, Sink, Source, Table, TablePart};

#[derive(Clone, Copy)]
pub(crate) struct DiskSource<'a> {
    pub store: &'a Store,
    pub spool: &'a Spool,
}
impl<'a> Source for DiskSource<'a> {
    type Text = TextRef;
    type Error = io::Error;
    type Lists = Lists<'a>;
    type Table = Parts<'a>;
    fn lists(&self) -> io::Result<Lists<'a>> {
        Ok(Lists {
            store: self.store,
            collection: self.store.collection()?,
            length: 0,
        })
    }
    fn table(&self) -> io::Result<Parts<'a>> {
        Ok(Parts {
            store: self.store,
            collection: self.store.collection()?,
            length: 0,
            bytes: 0,
        })
    }
    fn len(&self, text: &TextRef) -> u64 {
        text.len
    }
    fn slice(&self, text: &TextRef, range: Range<u64>) -> io::Result<TextRef> {
        self.spool.view(*text)?;
        text.subrange(range)
    }
    fn find(&self, text: &TextRef, byte: u8, from: u64) -> io::Result<Option<u64>> {
        let source = self.spool.view(*text)?;
        if from > text.len {
            return Err(invalid("invalid rich text search offset"));
        }
        let mut buffer = [0; 8192];
        let mut position = from;
        while position < text.len {
            let length = (text.len - position).min(buffer.len() as u64) as usize;
            exact(&source, position, &mut buffer[..length])?;
            if let Some(index) = buffer[..length].iter().position(|value| *value == byte) {
                return Ok(Some(position + index as u64));
            }
            position += length as u64;
        }
        Ok(None)
    }
    fn prefix_matches(
        &self,
        destination: &TextRef,
        offset: u64,
        text: &TextRef,
    ) -> io::Result<bool> {
        let destination_source = self.spool.view(*destination)?;
        let text_source = self.spool.view(*text)?;
        let Some(end) = offset
            .checked_add(text.len)
            .filter(|end| *end <= destination.len)
        else {
            return Ok(false);
        };
        let mut a = [0; 8192];
        let mut b = [0; 8192];
        let mut position = offset;
        while position < end {
            let length = (end - position).min(a.len() as u64) as usize;
            exact(&destination_source, position, &mut a[..length])?;
            exact(&text_source, position - offset, &mut b[..length])?;
            if a[..length] != b[..length] {
                return Ok(false);
            }
            position += length as u64;
        }
        Ok(true)
    }
}

pub(crate) struct Lists<'a> {
    store: &'a Store,
    collection: i64,
    length: u64,
}
impl ListStack for Lists<'_> {
    type Error = io::Error;
    fn len(&self) -> u64 {
        self.length
    }
    fn push(&mut self, value: Option<u64>) -> io::Result<()> {
        let next = self
            .length
            .checked_add(1)
            .ok_or_else(|| invalid("rich list depth overflow"))?;
        self.store.write(self.collection, self.length, &value)?;
        self.length = next;
        Ok(())
    }
    fn pop(&mut self) -> io::Result<()> {
        if self.length != 0 {
            self.store.truncate(self.collection, self.length - 1)?;
            self.length -= 1;
        }
        Ok(())
    }
    fn last(&self) -> io::Result<Option<Option<u64>>> {
        self.length
            .checked_sub(1)
            .map(|index| self.store.read(self.collection, index))
            .transpose()
    }
    fn set_last(&mut self, value: Option<u64>) -> io::Result<()> {
        self.store.write(
            self.collection,
            self.length
                .checked_sub(1)
                .ok_or_else(|| invalid("no open rich list"))?,
            &value,
        )
    }
}
impl Drop for Lists<'_> {
    fn drop(&mut self) {
        let _ = self.store.truncate(self.collection, 0);
    }
}

pub(crate) struct Parts<'a> {
    store: &'a Store,
    collection: i64,
    length: u64,
    bytes: u64,
}
impl Table<TextRef> for Parts<'_> {
    type Error = io::Error;
    fn push(&mut self, part: TablePart<TextRef>) -> io::Result<()> {
        let (value, bytes) = match part {
            TablePart::Text(text) => (Some(text), text.len),
            TablePart::Separator => (None, " │ ".len() as u64),
        };
        let length = self
            .length
            .checked_add(1)
            .ok_or_else(|| invalid("rich table length overflow"))?;
        let bytes = self
            .bytes
            .checked_add(bytes)
            .ok_or_else(|| invalid("rich table byte count overflow"))?;
        self.store.write(self.collection, self.length, &value)?;
        self.length = length;
        self.bytes = bytes;
        Ok(())
    }
    fn clear(&mut self) -> io::Result<()> {
        self.store.truncate(self.collection, 0)?;
        self.length = 0;
        self.bytes = 0;
        Ok(())
    }
    fn byte_len(&self) -> u64 {
        self.bytes
    }
    fn drain(&mut self) -> impl Iterator<Item = io::Result<TablePart<TextRef>>> {
        let mut index = 0;
        let length = self.length;
        self.length = 0;
        self.bytes = 0;
        std::iter::from_fn(move || {
            if index == length {
                return None;
            }
            let value = self
                .store
                .read::<Option<TextRef>>(self.collection, index)
                .map(|value| match value {
                    Some(text) => TablePart::Text(text),
                    None => TablePart::Separator,
                });
            index += 1;
            Some(value)
        })
    }
}
impl Drop for Parts<'_> {
    fn drop(&mut self) {
        let _ = self.store.truncate(self.collection, 0);
    }
}

pub(super) fn put<T: Record>(value: &T, bytes: &mut [u8], position: &mut usize) {
    value.encode(&mut bytes[*position..*position + T::SIZE]);
    *position += T::SIZE;
}
pub(super) fn take<T: Record>(bytes: &[u8], position: &mut usize) -> io::Result<T> {
    let value = T::decode(&bytes[*position..*position + T::SIZE])?;
    *position += T::SIZE;
    Ok(value)
}

#[derive(Clone, Copy)]
struct Run {
    kind: u64,
    text: Option<TextRef>,
    arg: u64,
    length: u64,
    style: StoredStyle,
}
impl Record for Run {
    const SIZE: usize = 8 + <Option<TextRef> as Record>::SIZE + 8 + 8 + StoredStyle::SIZE;
    fn encode(&self, bytes: &mut [u8]) {
        let mut position = 0;
        put(&self.kind, bytes, &mut position);
        put(&self.text, bytes, &mut position);
        put(&self.arg, bytes, &mut position);
        put(&self.length, bytes, &mut position);
        put(&self.style, bytes, &mut position);
    }
    fn decode(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() != Self::SIZE {
            return Err(invalid("invalid rich run size"));
        }
        let mut position = 0;
        let value = Self {
            kind: take(bytes, &mut position)?,
            text: take(bytes, &mut position)?,
            arg: take(bytes, &mut position)?,
            length: take(bytes, &mut position)?,
            style: take(bytes, &mut position)?,
        };
        if value.length == 0 || value.kind > 3 || (value.kind == 0) != value.text.is_some() {
            return Err(invalid("invalid rich run"));
        }
        Ok(value)
    }
}

#[derive(Clone, Copy)]
pub(crate) struct LineHead {
    pub runs: Option<u64>,
    pub bytes: u64,
    pub repeat: u64,
    pub glyphs: Option<u64>,
    pub cells: u64,
    prefix: Option<Run>,
}
impl Record for LineHead {
    const SIZE: usize = <Option<u64> as Record>::SIZE * 2 + 24 + <Option<Run> as Record>::SIZE;
    fn encode(&self, bytes: &mut [u8]) {
        let mut position = 0;
        put(&self.runs, bytes, &mut position);
        put(&self.bytes, bytes, &mut position);
        put(&self.repeat, bytes, &mut position);
        put(&self.glyphs, bytes, &mut position);
        put(&self.cells, bytes, &mut position);
        put(&self.prefix, bytes, &mut position);
    }
    fn decode(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() != Self::SIZE {
            return Err(invalid("invalid rich line size"));
        }
        let mut position = 0;
        let value = Self {
            runs: take(bytes, &mut position)?,
            bytes: take(bytes, &mut position)?,
            repeat: take(bytes, &mut position)?,
            glyphs: take(bytes, &mut position)?,
            cells: take(bytes, &mut position)?,
            prefix: take(bytes, &mut position)?,
        };
        let prefix = value.prefix.map_or(0, |run| run.length);
        if prefix > value.bytes
            || value.repeat == 0
            || (value.bytes != prefix && (value.runs.is_none() || value.repeat != 1))
            || value.prefix.is_some_and(|run| run.kind != 3)
        {
            return Err(invalid("invalid rich line"));
        }
        Ok(value)
    }
}
#[derive(Default)]
struct Current {
    collection: Option<i64>,
    bytes: u64,
    last: Option<(u64, Run)>,
    has_spans: bool,
}

pub(crate) struct Document<'a> {
    pub(super) source: DiskSource<'a>,
    headers: i64,
    pub(super) lines: u64,
    current: Current,
    literals: Vec<&'static str>,
    start: i64,
    end: Option<i64>,
    prefix: Option<(&'static str, &'static str, Style, bool)>,
}
impl<'a> Document<'a> {
    pub fn new(source: DiskSource<'a>) -> io::Result<Self> {
        let start = source.store.collection()?;
        let headers = source.store.collection()?;
        Ok(Self {
            source,
            headers,
            lines: 0,
            current: Current::default(),
            literals: Vec::new(),
            start,
            end: None,
            prefix: None,
        })
    }
    pub fn prefix(&mut self, value: Option<(&'static str, &'static str, Style)>) -> io::Result<()> {
        self.open()?;
        if self.current.has_spans {
            return Err(invalid("prefix changed inside a rich line"));
        }
        self.prefix = value.map(|(first, rest, style)| (first, rest, style, true));
        Ok(())
    }
    fn literal_index(&mut self, value: &'static str) -> u64 {
        match self.literals.iter().position(|literal| *literal == value) {
            Some(index) => index as u64,
            None => {
                let index = self.literals.len();
                self.literals.push(value);
                index as u64
            }
        }
    }
    pub fn seal(&mut self) -> io::Result<()> {
        if self.current.has_spans {
            return Err(invalid("unclosed rich line"));
        }
        self.end = Some(self.source.store.collection()?);
        Ok(())
    }
    fn open(&self) -> io::Result<()> {
        if self.end.is_some() {
            Err(invalid("sealed rich document"))
        } else {
            Ok(())
        }
    }
    fn append(&mut self, run: Run) -> io::Result<()> {
        self.open()?;
        self.current.has_spans = true;
        if run.length == 0 {
            return Ok(());
        }
        let next = self
            .current
            .bytes
            .checked_add(run.length)
            .ok_or_else(|| invalid("rich line byte count overflow"))?;
        let collection = match self.current.collection {
            Some(collection) => collection,
            None => {
                let collection = self.source.store.collection()?;
                self.current.collection = Some(collection);
                collection
            }
        };
        if let Some((position, mut previous)) = self.current.last {
            if previous.style.0 == run.style.0 && previous.kind == run.kind {
                let join = if run.kind == 0 {
                    match (previous.text, run.text) {
                        (Some(a), Some(b))
                            if a.source == b.source
                                && a.start.checked_add(a.len) == Some(b.start) =>
                        {
                            previous.text = Some(TextRef {
                                len: a
                                    .len
                                    .checked_add(b.len)
                                    .ok_or_else(|| invalid("rich run overflow"))?,
                                ..a
                            });
                            true
                        }
                        _ => false,
                    }
                } else if run.kind == 1 {
                    previous.arg = previous
                        .arg
                        .checked_add(run.arg)
                        .ok_or_else(|| invalid("rich space count overflow"))?;
                    true
                } else {
                    false
                };
                if join {
                    previous.length = previous
                        .length
                        .checked_add(run.length)
                        .ok_or_else(|| invalid("rich run length overflow"))?;
                    self.source.store.write(collection, position, &previous)?;
                    self.current.last = Some((position, previous));
                    self.current.bytes = next;
                    return Ok(());
                }
            }
        }
        self.source
            .store
            .write(collection, self.current.bytes, &run)?;
        self.current.last = Some((self.current.bytes, run));
        self.current.bytes = next;
        Ok(())
    }
    pub fn line(&self, index: u64) -> io::Result<(u64, LineHead)> {
        if index >= self.lines {
            return Err(invalid("unknown rich line"));
        }
        match self
            .source
            .store
            .floor_record::<LineHead>(self.headers, index)?
        {
            Some((first, head)) if index - first < head.repeat => Ok((first, head)),
            _ => Err(invalid("rich line disappeared")),
        }
    }
    pub(super) fn set_line(&self, first: u64, head: &LineHead) -> io::Result<()> {
        self.source.store.write(self.headers, first, head)
    }
    pub(super) fn headers(&self) -> impl Iterator<Item = io::Result<(u64, LineHead)>> + '_ {
        let mut after = None;
        let mut done = false;
        std::iter::from_fn(move || {
            if done {
                return None;
            }
            match self.source.store.next_record(self.headers, after) {
                Ok(Some((position, head))) => {
                    after = Some(position);
                    Some(Ok((position, head)))
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
    pub fn line_source(&self, head: LineHead) -> LineSource<'_, 'a> {
        LineSource {
            document: self,
            head,
            cached: RefCell::new(None),
        }
    }
    fn push_line(&mut self, mut head: LineHead) -> io::Result<()> {
        if let Some((first, rest, style, initial)) = self.prefix {
            self.prefix = Some((first, rest, style, false));
            let mut literal = if initial { first } else { rest };
            if initial && head.repeat > 1 {
                let repeat = head.repeat;
                head.repeat = 1;
                let mut first_head = head;
                first_head.prefix = (!literal.is_empty()).then(|| Run {
                    kind: 3,
                    text: None,
                    arg: self.literal_index(literal),
                    length: literal.len() as u64,
                    style: StoredStyle(style),
                });
                first_head.bytes = head
                    .bytes
                    .checked_add(literal.len() as u64)
                    .ok_or_else(|| invalid("rich prefix overflow"))?;
                self.push_line_raw(first_head)?;
                head.repeat = repeat - 1;
                literal = rest;
            }
            head.prefix = (!literal.is_empty()).then(|| Run {
                kind: 3,
                text: None,
                arg: self.literal_index(literal),
                length: literal.len() as u64,
                style: StoredStyle(style),
            });
            head.bytes = head
                .bytes
                .checked_add(literal.len() as u64)
                .ok_or_else(|| invalid("rich prefix overflow"))?;
        }
        self.push_line_raw(head)
    }
    fn push_line_raw(&mut self, head: LineHead) -> io::Result<()> {
        let next = self
            .lines
            .checked_add(head.repeat)
            .ok_or_else(|| invalid("rich line count overflow"))?;
        self.source.store.write(self.headers, self.lines, &head)?;
        self.lines = next;
        Ok(())
    }
}
impl Drop for Document<'_> {
    fn drop(&mut self) {
        let end = self.end.or_else(|| self.source.store.collection().ok());
        if let Some(end) = end {
            let _ = self.source.store.clear_collections(self.start, end);
        }
    }
}
impl Sink<TextRef> for Document<'_> {
    type Error = io::Error;
    fn text(&mut self, value: TextRef, style: Style) -> io::Result<()> {
        self.source.spool.view(value)?;
        self.append(Run {
            kind: 0,
            text: Some(value),
            arg: 0,
            length: value.len,
            style: StoredStyle(style),
        })
    }
    fn literal(&mut self, value: &'static str, style: Style) -> io::Result<()> {
        let index = self.literal_index(value);
        self.append(Run {
            kind: 3,
            text: None,
            arg: index,
            length: value.len() as u64,
            style: StoredStyle(style),
        })
    }
    fn spaces(&mut self, count: u64, style: Style) -> io::Result<()> {
        self.append(Run {
            kind: 1,
            text: None,
            arg: count,
            length: count,
            style: StoredStyle(style),
        })
    }
    fn number(&mut self, value: u64, style: Style) -> io::Result<()> {
        self.append(Run {
            kind: 2,
            text: None,
            arg: value,
            length: decimal(value).1 as u64,
            style: StoredStyle(style),
        })
    }
    fn blank_lines(&mut self, count: u64) -> io::Result<()> {
        self.open()?;
        if count != 0 {
            self.push_line(LineHead {
                runs: None,
                bytes: 0,
                repeat: count,
                glyphs: None,
                cells: 0,
                prefix: None,
            })?;
        }
        Ok(())
    }
    fn end_line(&mut self) -> io::Result<()> {
        self.open()?;
        let current = std::mem::take(&mut self.current);
        self.push_line(LineHead {
            runs: current.collection.map(|value| value as u64),
            bytes: current.bytes,
            repeat: 1,
            glyphs: None,
            cells: 0,
            prefix: None,
        })
    }
    fn standalone_line(
        &mut self,
        parts: impl Iterator<Item = io::Result<TablePart<TextRef>>>,
        _: u64,
        style: Style,
        leading_blanks: u64,
    ) -> io::Result<()> {
        self.open()?;
        let current = std::mem::take(&mut self.current);
        self.blank_lines(leading_blanks)?;
        for part in parts {
            match part? {
                TablePart::Text(value) => self.text(value, style)?,
                TablePart::Separator => self.literal(" │ ", style)?,
            }
        }
        self.end_line()?;
        self.current = current;
        Ok(())
    }
    fn standalone_literal(
        &mut self,
        value: &'static str,
        style: Style,
        leading_blanks: u64,
    ) -> io::Result<()> {
        self.open()?;
        let current = std::mem::take(&mut self.current);
        self.blank_lines(leading_blanks)?;
        self.literal(value, style)?;
        self.end_line()?;
        self.current = current;
        Ok(())
    }
}

pub(crate) struct LineSource<'d, 'a> {
    document: &'d Document<'a>,
    head: LineHead,
    cached: RefCell<Option<(u64, Run)>>,
}
impl LineSource<'_, '_> {
    fn run(&self, position: u64) -> io::Result<(u64, Run)> {
        let mut cached = self.cached.borrow_mut();
        let prefix = self.head.prefix.map_or(0, |run| run.length);
        if let Some(run) = self.head.prefix {
            if position < prefix {
                return Ok((0, run));
            }
        }
        if let Some((start, run)) = *cached {
            if position >= start && position - start < run.length {
                return Ok((start, run));
            }
        }
        let collection = self
            .head
            .runs
            .ok_or_else(|| invalid("rich line has no runs"))?;
        let (start, run) = self
            .document
            .source
            .store
            .floor_record::<Run>(
                i64::try_from(collection).map_err(io::Error::other)?,
                position - prefix,
            )?
            .ok_or_else(|| invalid("rich run disappeared"))?;
        let value = (start + prefix, run);
        if position - value.0 >= value.1.length {
            return Err(invalid("rich run range disappeared"));
        }
        *cached = Some(value);
        Ok(value)
    }
    pub fn style_at(&self, position: u64) -> io::Result<Style> {
        Ok(self.run(position)?.1.style.0)
    }
}
impl ByteSource for LineSource<'_, '_> {
    fn len(&self) -> u64 {
        self.head.bytes
    }
    fn read_at(&self, offset: u64, destination: &mut [u8]) -> io::Result<usize> {
        if offset > self.head.bytes {
            return Err(invalid("invalid rich line offset"));
        }
        let count = (self.head.bytes - offset).min(destination.len() as u64) as usize;
        let mut copied = 0;
        while copied < count {
            let (start, run) = self.run(offset + copied as u64)?;
            let local = offset + copied as u64 - start;
            let length = (count - copied).min((run.length - local) as usize);
            let target = &mut destination[copied..copied + length];
            match run.kind {
                0 => {
                    let value = run
                        .text
                        .ok_or_else(|| invalid("rich text reference missing"))?;
                    if value.len != run.length {
                        return Err(invalid("rich text run length mismatch"));
                    }
                    exact(&self.document.source.spool.view(value)?, local, target)?;
                }
                1 => {
                    if run.arg != run.length {
                        return Err(invalid("rich space run length mismatch"));
                    }
                    target.fill(b' ');
                }
                2 => {
                    let (bytes, size) = decimal(run.arg);
                    if size as u64 != run.length {
                        return Err(invalid("rich number run length mismatch"));
                    }
                    target.copy_from_slice(
                        &bytes[20 - size + local as usize..20 - size + local as usize + length],
                    );
                }
                3 => {
                    let literal = self
                        .document
                        .literals
                        .get(run.arg as usize)
                        .ok_or_else(|| invalid("rich literal missing"))?;
                    if literal.len() as u64 != run.length {
                        return Err(invalid("rich literal run length mismatch"));
                    }
                    target.copy_from_slice(
                        &literal.as_bytes()[local as usize..local as usize + length],
                    );
                }
                _ => return Err(invalid("unknown rich run kind")),
            }
            copied += length;
        }
        Ok(count)
    }
}

fn decimal(mut value: u64) -> ([u8; 20], usize) {
    let mut bytes = [0; 20];
    let mut start = 20;
    loop {
        start -= 1;
        bytes[start] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    (bytes, 20 - start)
}
pub(super) fn exact(
    source: &impl ByteSource,
    offset: u64,
    destination: &mut [u8],
) -> io::Result<()> {
    let mut copied = 0;
    while copied < destination.len() {
        let count = source.read_at(offset + copied as u64, &mut destination[copied..])?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "rich text bytes disappeared",
            ));
        }
        copied += count;
    }
    Ok(())
}
pub(super) fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
