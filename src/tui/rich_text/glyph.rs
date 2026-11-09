use std::{io, ops::Range};

use pulldown_cmark::disk::{ByteSource, Record};
use unicode_segmentation::{GraphemeCursor, GraphemeIncomplete};
use unicode_width::WidthAccumulator;

use super::disk::{exact, invalid, put, take, Document, LineHead, LineSource};
use super::wrap::{Block, Glyphs};

const CHUNK_BYTES: usize = 8192;

#[derive(Default)]
struct Chunk {
    start: u64,
    text: String,
}
impl Chunk {
    fn end(&self) -> u64 {
        self.start + self.text.len() as u64
    }
    fn contains(&self, position: u64) -> bool {
        position >= self.start && position < self.end()
    }
    fn range(&self, range: Range<u64>) -> Option<&str> {
        if range.start < self.start || range.end > self.end() {
            return None;
        }
        self.text
            .get((range.start - self.start) as usize..(range.end - self.start) as usize)
    }
    fn read(
        &mut self,
        source: &impl ByteSource,
        start: u64,
        end: u64,
        trailing_partial: bool,
    ) -> io::Result<()> {
        let mut bytes = std::mem::take(&mut self.text).into_bytes();
        if bytes.capacity() < CHUNK_BYTES {
            bytes.reserve_exact(CHUNK_BYTES - bytes.len());
        }
        bytes.resize((end - start) as usize, 0);
        exact(source, start, &mut bytes)?;
        self.text = match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(error) if trailing_partial && error.utf8_error().error_len().is_none() => {
                let valid = error.utf8_error().valid_up_to();
                let mut bytes = error.into_bytes();
                bytes.truncate(valid);
                String::from_utf8(bytes).map_err(|_| invalid("invalid rich UTF-8 prefix"))?
            }
            Err(_) => return Err(invalid("invalid rich UTF-8")),
        };
        self.start = start;
        Ok(())
    }
    fn forward(&mut self, source: &impl ByteSource, position: u64) -> io::Result<()> {
        if self.contains(position) {
            return Ok(());
        }
        let mut start = position.saturating_sub(1);
        if position != 0 {
            let mut continuation = 0;
            while source
                .byte_at(start)?
                .is_some_and(|byte| byte & 0xc0 == 0x80)
            {
                if start == 0 || continuation == 3 {
                    return Err(invalid("invalid rich UTF-8 boundary"));
                }
                continuation += 1;
                start -= 1;
            }
        }
        self.read(
            source,
            start,
            source.len().min(start + CHUNK_BYTES as u64),
            true,
        )?;
        if !self.contains(position) {
            return Err(invalid("incomplete rich UTF-8 scalar"));
        }
        Ok(())
    }
    fn ending(&mut self, source: &impl ByteSource, minimum: u64, end: u64) -> io::Result<()> {
        let mut start = minimum.max(end.saturating_sub(CHUNK_BYTES as u64));
        while start < end
            && source
                .byte_at(start)?
                .is_some_and(|byte| byte & 0xc0 == 0x80)
        {
            start += 1;
        }
        self.read(source, start, end, false)
    }
}

struct Boundaries<'a, S> {
    source: &'a S,
    cursor: GraphemeCursor,
    forward: Chunk,
    context: Chunk,
}
impl<'a, S: ByteSource> Boundaries<'a, S> {
    fn new(source: &'a S, mut forward: Chunk, mut context: Chunk) -> io::Result<Self> {
        let length =
            usize::try_from(source.len()).map_err(|_| invalid("rich grapheme offset overflow"))?;
        forward.text.clear();
        context.text.clear();
        Ok(Self {
            source,
            cursor: GraphemeCursor::new(0, length, true),
            forward,
            context,
        })
    }
    fn position(&self) -> u64 {
        self.cursor.cur_cursor() as u64
    }
    fn into_chunks(self) -> (Chunk, Chunk) {
        (self.forward, self.context)
    }
    fn skip_ascii(&mut self, end: u64) {
        self.cursor = GraphemeCursor::new(end as usize, self.source.len() as usize, true);
    }
    fn ascii_end(&mut self) -> io::Result<Option<u64>> {
        let position = self.position();
        self.forward.forward(self.source, position)?;
        let local = (position - self.forward.start) as usize;
        let bytes = &self.forward.text.as_bytes()[local..];
        let mut count = bytes
            .iter()
            .take_while(|byte| byte.is_ascii() && !byte.is_ascii_control())
            .count();
        if count == 0 {
            return Ok(None);
        }
        let next = position + count as u64;
        if self
            .source
            .byte_at(next)?
            .is_some_and(|byte| !byte.is_ascii())
        {
            count -= 1;
        }
        Ok((count != 0).then_some(position + count as u64))
    }
    fn next(&mut self) -> io::Result<Option<Range<u64>>> {
        let start = self.position();
        if start == self.source.len() {
            return Ok(None);
        }
        self.forward.forward(self.source, start)?;
        loop {
            match self
                .cursor
                .next_boundary(&self.forward.text, self.forward.start as usize)
            {
                Ok(Some(end)) => return Ok(Some(start..end as u64)),
                Ok(None) => return Ok(None),
                Err(GraphemeIncomplete::NextChunk) => {
                    // Overlap avoids recounting cached RI scalars; context remains unbounded.
                    self.forward.forward(self.source, self.position())?;
                }
                Err(GraphemeIncomplete::PreContext(end)) => {
                    let end = end as u64;
                    if end > self.forward.start && end <= self.forward.end() {
                        let text = self
                            .forward
                            .range(self.forward.start..end)
                            .ok_or_else(|| invalid("invalid grapheme context"))?;
                        self.cursor
                            .provide_context(text, self.forward.start as usize);
                    } else {
                        self.context.ending(self.source, 0, end)?;
                        self.cursor
                            .provide_context(&self.context.text, self.context.start as usize);
                    }
                }
                Err(_) => return Err(invalid("invalid rich grapheme cursor")),
            }
        }
    }
}

fn add_width(text: &str, width: &mut WidthAccumulator, marks: &mut u16, control: &mut bool) {
    for value in text.chars().rev() {
        *control |= value.is_control();
        if matches!(value, '\u{ff9e}' | '\u{ff9f}') {
            *marks = marks.wrapping_add(1);
        }
        width.push_reverse(value);
    }
}
fn width<S: ByteSource>(
    source: &S,
    range: Range<u64>,
    forward: &Chunk,
    scratch: &mut Chunk,
) -> io::Result<u64> {
    let mut width = WidthAccumulator::new();
    let mut marks = 0u16;
    let mut control = false;
    if let Some(text) = forward.range(range.clone()) {
        add_width(text, &mut width, &mut marks, &mut control);
    } else {
        let mut end = range.end;
        while end > range.start {
            scratch.ending(source, range.start, end)?;
            if scratch.text.is_empty() {
                return Err(invalid("empty reverse grapheme chunk"));
            }
            add_width(&scratch.text, &mut width, &mut marks, &mut control);
            end = scratch.start;
        }
    }
    Ok(if control {
        0
    } else if range.end - range.start == 1 {
        1
    } else {
        u64::from((width.width() as u16).saturating_add(marks))
    })
}

#[derive(Clone, Copy)]
struct Entry {
    end: u64,
    unit: u64,
    cells: u64,
    ascii: u64,
}
impl Record for Entry {
    const SIZE: usize = 32;
    fn encode(&self, bytes: &mut [u8]) {
        let mut position = 0;
        put(&self.end, bytes, &mut position);
        put(&self.unit, bytes, &mut position);
        put(&self.cells, bytes, &mut position);
        put(&self.ascii, bytes, &mut position);
    }
    fn decode(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() != Self::SIZE {
            return Err(invalid("invalid glyph entry size"));
        }
        let mut position = 0;
        let entry = Self {
            end: take(bytes, &mut position)?,
            unit: take(bytes, &mut position)?,
            cells: take(bytes, &mut position)?,
            ascii: take(bytes, &mut position)?,
        };
        if entry.ascii > 1
            || (entry.cells != 0 && entry.unit == 0)
            || (entry.ascii == 1 && (entry.unit != 1 || entry.cells != 1))
        {
            return Err(invalid("invalid glyph entry"));
        }
        Ok(entry)
    }
}

pub(crate) fn index(document: &Document<'_>, current: impl Fn() -> bool) -> io::Result<()> {
    let store = document.source.store;
    let mut forward = Chunk::default();
    let mut context = Chunk::default();
    let mut reverse = Chunk::default();
    for line in document.headers() {
        if !current() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "retained glyph indexing superseded",
            ));
        }
        let (first, mut head) = line?;
        if head.bytes == 0 || head.glyphs.is_some() {
            continue;
        }
        let collection = store.collection()?;
        let source = document.line_source(head);
        let mut boundaries = Boundaries::new(
            &source,
            std::mem::take(&mut forward),
            std::mem::take(&mut context),
        )?;
        let mut pending: Option<(u64, Entry)> = None;
        let mut total = 0u64;
        while boundaries.position() < source.len() {
            if !current() {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "retained glyph indexing superseded",
                ));
            }
            let start = boundaries.position();
            let (end, unit, cells, ascii) = match boundaries.ascii_end()? {
                Some(end) => {
                    boundaries.skip_ascii(end);
                    (end, 1, 1, 1)
                }
                None => {
                    let range = boundaries
                        .next()?
                        .ok_or_else(|| invalid("rich grapheme disappeared"))?;
                    let cells = width(&source, range.clone(), &boundaries.forward, &mut reverse)?;
                    let ascii = u64::from(
                        range.end - range.start == 1
                            && source
                                .byte_at(range.start)?
                                .is_some_and(|byte| byte.is_ascii() && !byte.is_ascii_control()),
                    );
                    (
                        range.end,
                        if cells == 0 {
                            0
                        } else {
                            range.end - range.start
                        },
                        cells,
                        ascii,
                    )
                }
            };
            let count = if cells == 0 { 0 } else { (end - start) / unit };
            total = total
                .checked_add(
                    count
                        .checked_mul(cells)
                        .ok_or_else(|| invalid("glyph width overflow"))?,
                )
                .ok_or_else(|| invalid("line width overflow"))?;
            let entry = Entry {
                end,
                unit,
                cells,
                ascii,
            };
            match pending.as_mut() {
                Some((_, previous))
                    if previous.end == start
                        && ((previous.cells == 0 && cells == 0)
                            || (previous.unit == unit
                                && previous.cells == cells
                                && previous.ascii == ascii)) =>
                {
                    previous.end = end
                }
                _ => {
                    if let Some((start, previous)) = pending {
                        store.write(collection, start, &previous)?;
                    }
                    pending = Some((start, entry));
                }
            }
        }
        (forward, context) = boundaries.into_chunks();
        if let Some((start, entry)) = pending {
            store.write(collection, start, &entry)?;
        }
        head.glyphs = Some(collection as u64);
        head.cells = total;
        document.set_line(first, &head)?;
    }
    Ok(())
}

pub(crate) struct DiskGlyphs<'d, 'a> {
    document: &'d Document<'a>,
    source: LineSource<'d, 'a>,
    head: LineHead,
    cached: Option<Block>,
    search: [u8; CHUNK_BYTES],
}
impl<'d, 'a> DiskGlyphs<'d, 'a> {
    pub fn new(document: &'d Document<'a>, head: LineHead) -> Self {
        Self {
            document,
            source: document.line_source(head),
            head,
            cached: None,
            search: [0; CHUNK_BYTES],
        }
    }
}
impl Glyphs for DiskGlyphs<'_, '_> {
    fn len(&self) -> u64 {
        self.head.bytes
    }
    fn cells(&self) -> u64 {
        self.head.cells
    }
    fn block(&mut self, position: u64) -> io::Result<Block> {
        if let Some(block) = self.cached {
            if position >= block.start && position < block.end {
                return Ok(block);
            }
        }
        let collection = self
            .head
            .glyphs
            .ok_or_else(|| invalid("rich line glyphs unavailable"))?;
        let (start, entry) = self
            .document
            .source
            .store
            .floor_record::<Entry>(
                i64::try_from(collection).map_err(io::Error::other)?,
                position,
            )?
            .ok_or_else(|| invalid("glyph entry disappeared"))?;
        let block = Block {
            start,
            end: entry.end,
            unit: entry.unit,
            cells: entry.cells,
            ascii: entry.ascii == 1,
        };
        self.cached = Some(block);
        Ok(block)
    }
    fn last_space(&mut self, range: Range<u64>) -> io::Result<Option<u64>> {
        let mut end = range.end;
        while end > range.start {
            let count = (end - range.start).min(self.search.len() as u64) as usize;
            let start = end - count as u64;
            exact(&self.source, start, &mut self.search[..count])?;
            if let Some(index) = self.search[..count].iter().rposition(|byte| *byte == b' ') {
                return Ok(Some(start + index as u64));
            }
            end = start;
        }
        Ok(None)
    }
}
