use std::cell::Cell;
use std::io::{self, Read, Write};

use super::records::{put, take};
use pulldown_cmark::disk::{ByteSource, Record, Sequence, Spool, Store, TextRef};
use serde::de::IgnoredAny;

const READ_BYTES: usize = 8192;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    String,
    Number,
    False,
    True,
    Null,
    Object,
    Array,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ValueRef {
    collection: i64,
    index: u64,
}

impl Record for ValueRef {
    const SIZE: usize = 16;
    fn encode(&self, bytes: &mut [u8]) {
        (self.collection as u64, self.index).encode(bytes);
    }
    fn decode(bytes: &[u8]) -> io::Result<Self> {
        let (collection, index) = <(u64, u64)>::decode(bytes)?;
        Ok(Self {
            collection: i64::try_from(collection)
                .map_err(|_| invalid("JSON collection overflow"))?,
            index,
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) struct Node {
    pub kind: Kind,
    pub value: TextRef,
    pub key: Option<TextRef>,
    first: Option<u64>,
    last: Option<u64>,
    next: Option<u64>,
    pub count: u64,
}

impl Record for Node {
    const SIZE: usize = 8 + TextRef::SIZE + Option::<TextRef>::SIZE + 3 * Option::<u64>::SIZE + 8;
    fn encode(&self, bytes: &mut [u8]) {
        let mut position = 0;
        put(&(self.kind as u64), bytes, &mut position);
        put(&self.value, bytes, &mut position);
        put(&self.key, bytes, &mut position);
        put(&self.first, bytes, &mut position);
        put(&self.last, bytes, &mut position);
        put(&self.next, bytes, &mut position);
        put(&self.count, bytes, &mut position);
    }
    fn decode(bytes: &[u8]) -> io::Result<Self> {
        let mut position = 0;
        let kind = match take::<u64>(bytes, &mut position)? {
            0 => Kind::String,
            1 => Kind::Number,
            2 => Kind::False,
            3 => Kind::True,
            4 => Kind::Null,
            5 => Kind::Object,
            6 => Kind::Array,
            _ => return Err(invalid("invalid JSON node kind")),
        };
        Ok(Self {
            kind,
            value: take(bytes, &mut position)?,
            key: take(bytes, &mut position)?,
            first: take(bytes, &mut position)?,
            last: take(bytes, &mut position)?,
            next: take(bytes, &mut position)?,
            count: take(bytes, &mut position)?,
        })
    }
}

const ARRAY_FIRST: u64 = 0;
const ARRAY_VALUE: u64 = 1;
const ARRAY_COMMA: u64 = 2;
const OBJECT_FIRST: u64 = 3;
const OBJECT_KEY: u64 = 4;
const OBJECT_COLON: u64 = 5;
const OBJECT_VALUE: u64 = 6;
const OBJECT_COMMA: u64 = 7;

#[derive(Clone, Copy)]
struct Frame {
    node: u64,
    state: u64,
    key: Option<TextRef>,
}
impl Record for Frame {
    const SIZE: usize = <(u64, (u64, Option<TextRef>))>::SIZE;
    fn encode(&self, bytes: &mut [u8]) {
        (self.node, (self.state, self.key)).encode(bytes);
    }
    fn decode(bytes: &[u8]) -> io::Result<Self> {
        let (node, (state, key)) = <(u64, (u64, Option<TextRef>))>::decode(bytes)?;
        if state > OBJECT_COMMA {
            return Err(invalid("invalid JSON container state"));
        }
        Ok(Self { node, state, key })
    }
}

#[derive(Debug)]
pub(super) struct Arena {
    collection: i64,
    length: Cell<u64>,
}

impl Arena {
    pub fn new(store: &Store) -> io::Result<Self> {
        Ok(Self {
            collection: store.collection()?,
            length: Cell::new(0),
        })
    }

    pub fn node(&self, store: &Store, value: ValueRef) -> io::Result<Node> {
        if value.collection != self.collection || value.index >= self.length.get() {
            return Err(invalid("unknown JSON value"));
        }
        store.read(self.collection, value.index)
    }

    pub fn children<'a>(&'a self, store: &'a Store, value: ValueRef) -> io::Result<Children<'a>> {
        let node = self.node(store, value)?;
        if !matches!(node.kind, Kind::Object | Kind::Array) {
            return Err(invalid("JSON container expected"));
        }
        Ok(Children {
            arena: self,
            store,
            next: node.first,
        })
    }

    pub fn property(
        &self,
        store: &Store,
        spool: &Spool,
        value: ValueRef,
        name: &str,
    ) -> io::Result<Option<ValueRef>> {
        if self.node(store, value)?.kind != Kind::Object {
            return Err(invalid("JSON object expected"));
        }
        let mut found = None;
        for child in self.children(store, value)? {
            let child = child?;
            let node = self.node(store, child)?;
            if let Some(key) = node.key {
                if text_is(spool, key, name)? {
                    if found.is_some() {
                        return Err(invalid("duplicate JSON property"));
                    }
                    found = Some(child);
                }
            }
        }
        Ok(found)
    }

    pub fn text(&self, store: &Store, value: ValueRef) -> io::Result<TextRef> {
        let node = self.node(store, value)?;
        if node.kind != Kind::String {
            return Err(invalid("JSON string expected"));
        }
        Ok(node.value)
    }

    pub fn boolean(&self, store: &Store, value: ValueRef) -> io::Result<bool> {
        match self.node(store, value)?.kind {
            Kind::True => Ok(true),
            Kind::False => Ok(false),
            _ => Err(invalid("JSON boolean expected")),
        }
    }

    pub fn unsigned(&self, store: &Store, spool: &Spool, value: ValueRef) -> io::Result<u64> {
        let node = self.node(store, value)?;
        if node.kind != Kind::Number || node.value.len == 0 || node.value.len > 20 {
            return Err(invalid("JSON unsigned integer expected"));
        }
        let mut value = 0u64;
        for index in 0..node.value.len {
            let byte = byte(spool, node.value, index)?;
            if !byte.is_ascii_digit() {
                return Err(invalid("JSON unsigned integer expected"));
            }
            value = value
                .checked_mul(10)
                .and_then(|value| value.checked_add(u64::from(byte - b'0')))
                .ok_or_else(|| invalid("JSON unsigned integer overflow"))?;
        }
        Ok(value)
    }

    pub fn parse(&self, store: &Store, spool: &Spool, input: TextRef) -> io::Result<ValueRef> {
        spool.view(input)?;
        usize::try_from(input.len).map_err(|_| invalid("JSON source offset overflow"))?;
        let mark = self.length.get();
        let mut stack = Sequence::<Frame>::new(store)?;
        let result = spool.with_speculation(|| {
            let mut reader = Reader::new(spool, input);
            let mut root = None;
            loop {
                reader.whitespace()?;
                if stack.is_empty() {
                    if let Some(root) = root {
                        if reader.position != input.len {
                            return Err(invalid("trailing JSON content"));
                        }
                        return Ok(Some(root));
                    }
                    root = Some(self.value(store, &mut reader, &mut stack, None, None)?);
                    continue;
                }
                let index = stack.len() - 1;
                let mut frame = stack.get(index)?;
                match frame.state {
                    ARRAY_FIRST | ARRAY_VALUE => {
                        if frame.state == ARRAY_FIRST && reader.peek()? == Some(b']') {
                            self.close(store, &mut reader, &mut stack, frame)?;
                        } else {
                            frame.state = ARRAY_COMMA;
                            stack.set(index, &frame)?;
                            self.value(store, &mut reader, &mut stack, Some(frame.node), None)?;
                        }
                    }
                    OBJECT_FIRST | OBJECT_KEY => {
                        if frame.state == OBJECT_FIRST && reader.peek()? == Some(b'}') {
                            self.close(store, &mut reader, &mut stack, frame)?;
                        } else {
                            if reader.peek()? != Some(b'"') {
                                return Err(invalid("JSON object key expected"));
                            }
                            let (raw, escaped) = reader.primitive()?;
                            frame.key = Some(decode_string(spool, raw, escaped)?);
                            frame.state = OBJECT_COLON;
                            stack.set(index, &frame)?;
                        }
                    }
                    OBJECT_COLON => {
                        if reader.peek()? != Some(b':') {
                            return Err(invalid("JSON colon expected"));
                        }
                        reader.position += 1;
                        frame.state = OBJECT_VALUE;
                        stack.set(index, &frame)?;
                    }
                    OBJECT_VALUE => {
                        let key = frame.key.take();
                        frame.state = OBJECT_COMMA;
                        stack.set(index, &frame)?;
                        self.value(store, &mut reader, &mut stack, Some(frame.node), key)?;
                    }
                    ARRAY_COMMA | OBJECT_COMMA => match reader.peek()? {
                        Some(b',') => {
                            reader.position += 1;
                            frame.state = if frame.state == ARRAY_COMMA {
                                ARRAY_VALUE
                            } else {
                                OBJECT_KEY
                            };
                            stack.set(index, &frame)?;
                        }
                        Some(b']') if frame.state == ARRAY_COMMA => {
                            self.close(store, &mut reader, &mut stack, frame)?
                        }
                        Some(b'}') if frame.state == OBJECT_COMMA => {
                            self.close(store, &mut reader, &mut stack, frame)?
                        }
                        _ => return Err(invalid("JSON comma or container end expected")),
                    },
                    _ => return Err(invalid("invalid JSON parser state")),
                }
            }
        });
        match result {
            Ok(Some(value)) => Ok(value),
            Ok(None) => Err(invalid("JSON parser returned no value")),
            Err(error) => {
                self.length.set(mark);
                store.truncate(self.collection, mark)?;
                stack.truncate(0)?;
                Err(error)
            }
        }
    }

    fn value(
        &self,
        store: &Store,
        reader: &mut Reader<'_>,
        stack: &mut Sequence<'_, Frame>,
        parent: Option<u64>,
        key: Option<TextRef>,
    ) -> io::Result<ValueRef> {
        let start = reader.position;
        let first = reader
            .peek()?
            .ok_or_else(|| invalid("JSON value expected"))?;
        let (kind, raw) = match first {
            b'{' | b'[' => {
                reader.position += 1;
                (
                    if first == b'{' {
                        Kind::Object
                    } else {
                        Kind::Array
                    },
                    reader.input.subrange(start..reader.position)?,
                )
            }
            b'"' | b'-' | b'0'..=b'9' | b'n' | b't' | b'f' => {
                let (raw, escaped) = reader.primitive()?;
                match first {
                    b'"' => (Kind::String, decode_string(reader.spool, raw, escaped)?),
                    b'n' => (Kind::Null, raw),
                    b't' => (Kind::True, raw),
                    b'f' => (Kind::False, raw),
                    _ => (Kind::Number, raw),
                }
            }
            _ => return Err(invalid("JSON value expected")),
        };
        let index = self.length.get();
        let next = index
            .checked_add(1)
            .ok_or_else(|| invalid("JSON node index overflow"))?;
        let node = Node {
            kind,
            value: raw,
            key,
            first: None,
            last: None,
            next: None,
            count: 0,
        };
        store.write(self.collection, index, &node)?;
        self.length.set(next);
        if let Some(parent) = parent {
            let mut parent_node: Node = store.read(self.collection, parent)?;
            if let Some(last) = parent_node.last {
                let mut previous: Node = store.read(self.collection, last)?;
                previous.next = Some(index);
                store.write(self.collection, last, &previous)?;
            } else {
                parent_node.first = Some(index);
            }
            parent_node.last = Some(index);
            parent_node.count = parent_node
                .count
                .checked_add(1)
                .ok_or_else(|| invalid("JSON child count overflow"))?;
            store.write(self.collection, parent, &parent_node)?;
        }
        if matches!(kind, Kind::Object | Kind::Array) {
            stack.push(&Frame {
                node: index,
                state: if kind == Kind::Object {
                    OBJECT_FIRST
                } else {
                    ARRAY_FIRST
                },
                key: None,
            })?;
        }
        Ok(ValueRef {
            collection: self.collection,
            index,
        })
    }

    fn close(
        &self,
        store: &Store,
        reader: &mut Reader<'_>,
        stack: &mut Sequence<'_, Frame>,
        frame: Frame,
    ) -> io::Result<()> {
        reader.position += 1;
        let mut node: Node = store.read(self.collection, frame.node)?;
        node.value.len = reader.input.start + reader.position - node.value.start;
        store.write(self.collection, frame.node, &node)?;
        stack.pop()?;
        Ok(())
    }
}

pub(super) struct Children<'a> {
    arena: &'a Arena,
    store: &'a Store,
    next: Option<u64>,
}
impl Iterator for Children<'_> {
    type Item = io::Result<ValueRef>;
    fn next(&mut self) -> Option<Self::Item> {
        let index = self.next.take()?;
        let value = ValueRef {
            collection: self.arena.collection,
            index,
        };
        match self.arena.node(self.store, value) {
            Ok(node) => {
                self.next = node.next;
                Some(Ok(value))
            }
            Err(error) => Some(Err(error)),
        }
    }
}

pub(super) fn text_is(spool: &Spool, value: TextRef, expected: &str) -> io::Result<bool> {
    if value.len != expected.len() as u64 {
        return Ok(false);
    }
    let source = spool.view(value)?;
    let mut offset = 0;
    let mut buffer = [0; 256];
    while offset < expected.len() {
        let wanted = (expected.len() - offset).min(buffer.len());
        let count = source.read_at(offset as u64, &mut buffer[..wanted])?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "JSON text disappeared",
            ));
        }
        if buffer[..count] != expected.as_bytes()[offset..offset + count] {
            return Ok(false);
        }
        offset += count;
    }
    Ok(true)
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn byte(spool: &Spool, value: TextRef, index: u64) -> io::Result<u8> {
    if index >= value.len {
        return Err(invalid("incomplete JSON string escape"));
    }
    spool
        .byte_at(value.start + index)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "JSON source disappeared"))
}
fn hex4(spool: &Spool, value: TextRef, start: u64) -> io::Result<u32> {
    let mut result = 0;
    for index in start..start + 4 {
        let byte = byte(spool, value, index)?;
        let digit = match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'f' => byte - b'a' + 10,
            b'A'..=b'F' => byte - b'A' + 10,
            _ => return Err(invalid("invalid JSON Unicode escape")),
        };
        result = (result << 4) | u32::from(digit);
    }
    Ok(result)
}
fn decode_string(spool: &Spool, raw: TextRef, escaped: bool) -> io::Result<TextRef> {
    let raw = raw.subrange(1..raw.len - 1)?;
    if !escaped {
        return Ok(raw);
    }
    let mut writer = spool.writer();
    let mut mark = 0;
    let mut index = 0;
    while index < raw.len {
        if byte(spool, raw, index)? != b'\\' {
            index += 1;
            continue;
        }
        spool.copy_to(raw.subrange(mark..index)?, &mut writer)?;
        index += 1;
        let value = match byte(spool, raw, index)? {
            b'"' => '"',
            b'\\' => '\\',
            b'/' => '/',
            b'b' => '\u{8}',
            b'f' => '\u{c}',
            b'n' => '\n',
            b'r' => '\r',
            b't' => '\t',
            b'u' => {
                let high = hex4(spool, raw, index + 1)?;
                index += 4;
                let scalar = if (0xD800..=0xDBFF).contains(&high) {
                    if byte(spool, raw, index + 1)? != b'\\' || byte(spool, raw, index + 2)? != b'u'
                    {
                        return Err(invalid("unpaired JSON high surrogate"));
                    }
                    let low = hex4(spool, raw, index + 3)?;
                    if !(0xDC00..=0xDFFF).contains(&low) {
                        return Err(invalid("invalid JSON surrogate pair"));
                    }
                    index += 6;
                    0x10000 + ((high - 0xD800) << 10) + low - 0xDC00
                } else {
                    high
                };
                char::from_u32(scalar).ok_or_else(|| invalid("invalid JSON Unicode scalar"))?
            }
            _ => return Err(invalid("invalid JSON string escape")),
        };
        let mut utf8 = [0; 4];
        writer.write_all(value.encode_utf8(&mut utf8).as_bytes())?;
        index += 1;
        mark = index;
    }
    spool.copy_to(raw.subrange(mark..raw.len)?, &mut writer)?;
    writer.finish()
}

#[derive(Default)]
struct Utf8 {
    pending: [u8; 4],
    length: usize,
    expected: usize,
}
impl Utf8 {
    fn push(&mut self, mut bytes: &[u8]) -> io::Result<()> {
        if self.length != 0 {
            let count = (self.expected - self.length).min(bytes.len());
            self.pending[self.length..self.length + count].copy_from_slice(&bytes[..count]);
            self.length += count;
            bytes = &bytes[count..];
            if self.length != self.expected {
                return Ok(());
            }
            std::str::from_utf8(&self.pending[..self.length])
                .map_err(|_| invalid("invalid JSON UTF-8"))?;
            self.length = 0;
        }
        if let Err(error) = std::str::from_utf8(bytes) {
            if error.error_len().is_some() {
                return Err(invalid("invalid JSON UTF-8"));
            }
            let pending = &bytes[error.valid_up_to()..];
            self.pending[..pending.len()].copy_from_slice(pending);
            self.length = pending.len();
            self.expected = match pending[0] {
                0xC2..=0xDF => 2,
                0xE0..=0xEF => 3,
                0xF0..=0xF4 => 4,
                _ => return Err(invalid("invalid JSON UTF-8")),
            };
        }
        Ok(())
    }
    fn finish(&self) -> io::Result<()> {
        if self.length == 0 {
            Ok(())
        } else {
            Err(invalid("incomplete JSON UTF-8"))
        }
    }
}

struct Reader<'a> {
    spool: &'a Spool,
    input: TextRef,
    position: u64,
    start: u64,
    length: usize,
    buffer: [u8; READ_BYTES],
    validated: u64,
    utf8: Utf8,
    escaped: bool,
}
impl<'a> Reader<'a> {
    fn new(spool: &'a Spool, input: TextRef) -> Self {
        Self {
            spool,
            input,
            position: 0,
            start: 0,
            length: 0,
            buffer: [0; READ_BYTES],
            validated: 0,
            utf8: Utf8::default(),
            escaped: false,
        }
    }
    fn fill(&mut self) -> io::Result<()> {
        if self.position >= self.start && self.position < self.start + self.length as u64 {
            return Ok(());
        }
        let wanted = (self.input.len - self.position).min(READ_BYTES as u64) as usize;
        self.start = self.position;
        self.length = self
            .spool
            .read_at(self.input.start + self.position, &mut self.buffer[..wanted])?;
        if wanted != 0 && self.length == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "JSON source disappeared",
            ));
        }
        let end = self.start + self.length as u64;
        if end > self.validated {
            let from = self.validated.saturating_sub(self.start) as usize;
            self.utf8.push(&self.buffer[from..self.length])?;
            self.validated = end;
        }
        if self.validated == self.input.len {
            self.utf8.finish()?;
        }
        Ok(())
    }
    fn peek(&mut self) -> io::Result<Option<u8>> {
        if self.position == self.input.len {
            return Ok(None);
        }
        self.fill()?;
        Ok(Some(self.buffer[(self.position - self.start) as usize]))
    }
    fn whitespace(&mut self) -> io::Result<()> {
        while matches!(self.peek()?, Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.position += 1;
        }
        Ok(())
    }
    fn primitive(&mut self) -> io::Result<(TextRef, bool)> {
        let start = self.position;
        self.escaped = false;
        let mut stream =
            serde_json::Deserializer::from_reader(&mut *self).into_iter::<IgnoredAny>();
        stream
            .next()
            .ok_or_else(|| invalid("JSON value expected"))?
            .map_err(|error| {
                io::Error::new(
                    error.io_error_kind().unwrap_or(io::ErrorKind::InvalidData),
                    error,
                )
            })?;
        let count = stream.byte_offset() as u64;
        drop(stream);
        self.position = start + count;
        Ok((self.input.subrange(start..self.position)?, self.escaped))
    }
}
impl Read for Reader<'_> {
    fn read(&mut self, destination: &mut [u8]) -> io::Result<usize> {
        if destination.is_empty() || self.position == self.input.len {
            return Ok(0);
        }
        self.fill()?;
        let offset = (self.position - self.start) as usize;
        let count = destination.len().min(self.length - offset);
        let bytes = &self.buffer[offset..offset + count];
        self.escaped |= bytes.contains(&b'\\');
        destination[..count].copy_from_slice(bytes);
        self.position += count as u64;
        Ok(count)
    }
}
