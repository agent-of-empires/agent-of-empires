use std::{
    io::{self, Write},
    ops::Range,
};

use pulldown_cmark::disk::{ByteSource, Store};
use ratatui::text::{Line, Span, Text};

use super::{
    disk::{exact, invalid, Document},
    glyph::DiskGlyphs,
    wrap::{Breaks, Glyphs},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Anchor {
    pub line: u64,
    pub byte: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Row {
    pub line: u64,
    pub start: u64,
    pub end: u64,
}
#[derive(Clone, Copy)]
pub(crate) struct Selection {
    pub start: (u16, u64),
    pub end: (u16, u64),
}

// (logical first line, logical repetitions, wrapped rows per line, row collection).
type Segment = (u64, (u64, (u64, u64)));
// (first byte, end byte, byte stride, repeated wrapped rows).
type Entry = (u64, (u64, (u64, u64)));

pub(crate) struct Index<'a> {
    store: &'a Store,
    segments: i64,
    start: i64,
    end: Option<i64>,
    rows: u64,
    pub width: u16,
}
impl<'a> Index<'a> {
    pub fn build(
        document: &Document<'a>,
        width: u16,
        current: impl Fn() -> bool,
    ) -> io::Result<Self> {
        let store = document.source.store;
        let start = store.collection()?;
        let mut index = Self {
            store,
            segments: store.collection()?,
            start,
            end: None,
            rows: 0,
            width: width.max(1),
        };
        for line in document.headers() {
            if !current() {
                return Err(interrupted());
            }
            let (first, head) = line?;
            let collection = store.collection()?;
            let mut count = 0u64;
            let mut pending: Option<(u64, Entry)> = None;
            for range in Breaks::new(DiskGlyphs::new(document, head), index.width) {
                if !current() {
                    return Err(interrupted());
                }
                let range = range?;
                let mut joined = false;
                if let Some((_, (start, (end, (stride, repeat))))) = pending.as_mut() {
                    let last = start
                        .checked_add(
                            stride
                                .checked_mul(*repeat - 1)
                                .ok_or_else(|| invalid("wrapped row stride overflow"))?,
                        )
                        .ok_or_else(|| invalid("wrapped row offset overflow"))?;
                    if range.end - range.start == *end - *start
                        && range.start > last
                        && (*repeat == 1 || range.start - last == *stride)
                    {
                        if *repeat == 1 {
                            *stride = range.start - last;
                        }
                        *repeat += 1;
                        joined = true;
                    }
                }
                if !joined {
                    if let Some((position, entry)) = pending.take() {
                        store.write(collection, position, &entry)?;
                    }
                    pending = Some((count, (range.start, (range.end, (0, 1)))));
                }
                count = count
                    .checked_add(1)
                    .ok_or_else(|| invalid("wrapped line row count overflow"))?;
            }
            if let Some((position, entry)) = pending {
                store.write(collection, position, &entry)?;
            }
            if count == 0 {
                return Err(invalid("rich line has no wrapped rows"));
            }
            let segment: Segment = (first, (head.repeat, (count, collection as u64)));
            store.write(index.segments, index.rows, &segment)?;
            index.rows = index
                .rows
                .checked_add(
                    count
                        .checked_mul(head.repeat)
                        .ok_or_else(|| invalid("wrapped repeated line overflow"))?,
                )
                .ok_or_else(|| invalid("wrapped row count overflow"))?;
        }
        index.end = Some(store.collection()?);
        Ok(index)
    }
    pub fn len(&self) -> u64 {
        self.rows
    }
    pub fn row(&self, position: u64) -> io::Result<Row> {
        if position >= self.rows {
            return Err(invalid("unknown wrapped row"));
        }
        let (first, (line, (repeat, (per_line, collection)))) = self
            .store
            .floor_record::<Segment>(self.segments, position)?
            .ok_or_else(|| invalid("wrapped segment disappeared"))?;
        if per_line == 0 || (position - first) / per_line >= repeat {
            return Err(invalid("invalid wrapped segment"));
        }
        let local = (position - first) % per_line;
        let (start, (byte, (end, (stride, count)))) = self
            .store
            .floor_record::<Entry>(i64::try_from(collection).map_err(io::Error::other)?, local)?
            .ok_or_else(|| invalid("wrapped row disappeared"))?;
        if local - start >= count {
            return Err(invalid("wrapped row repeat disappeared"));
        }
        let offset = stride
            .checked_mul(local - start)
            .ok_or_else(|| invalid("wrapped row offset overflow"))?;
        Ok(Row {
            line: line + (position - first) / per_line,
            start: byte + offset,
            end: end + offset,
        })
    }
    pub fn locate(&self, anchor: Anchor) -> io::Result<u64> {
        let mut low = 0;
        let mut high = self.rows;
        while low < high {
            let middle = low + (high - low) / 2;
            let row = self.row(middle)?;
            if row.line < anchor.line
                || (row.line == anchor.line && row.end <= anchor.byte && row.start != row.end)
            {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        Ok(low.min(self.rows.saturating_sub(1)))
    }
    pub fn window(
        &self,
        document: &Document<'_>,
        range: Range<u64>,
        current: impl Fn() -> bool,
    ) -> io::Result<Text<'static>> {
        let mut lines = Vec::with_capacity(
            usize::try_from(range.end.saturating_sub(range.start)).map_err(io::Error::other)?,
        );
        let mut scratch = Vec::new();
        for position in range.start..range.end.min(self.rows) {
            if !current() {
                return Err(interrupted());
            }
            lines.push(paint(document, self.row(position)?, &mut scratch)?);
        }
        Ok(Text::from(lines))
    }
    pub fn copy_selection(
        &self,
        document: &Document<'_>,
        selection: Selection,
        output: &mut impl Write,
        current: impl Fn() -> bool,
    ) -> io::Result<bool> {
        let ((start_column, start), (end_column, end)) = (selection.start, selection.end);
        if start > end || end >= self.rows || (start == end && start_column >= end_column) {
            return Err(invalid("invalid retained selection"));
        }
        let mut scratch = Vec::new();
        let mut has_text = false;
        for position in start..=end {
            if !current() {
                return Err(interrupted());
            }
            let line = paint(document, self.row(position)?, &mut scratch)?;
            let from = if position == start { start_column } else { 0 };
            let to = if position == end {
                end_column.saturating_add(1).min(self.width)
            } else {
                self.width
            };
            let text =
                crate::tui::components::text::line_columns(&line, self.width).slice(from, to);
            let text = text.trim_end();
            has_text |= text.chars().any(|ch| !ch.is_whitespace());
            output.write_all(text.as_bytes())?;
            if position != end {
                output.write_all(b"\n")?;
            }
        }
        Ok(has_text)
    }
}
impl Drop for Index<'_> {
    fn drop(&mut self) {
        if let Some(end) = self.end.or_else(|| self.store.collection().ok()) {
            let _ = self.store.clear_collections(self.start, end);
        }
    }
}

pub(crate) fn copy_all(
    document: &Document<'_>,
    output: &mut impl Write,
    current: impl Fn() -> bool,
) -> io::Result<()> {
    let mut first = true;
    let mut buffer = [0; 8192];
    for line in document.headers() {
        let (_, head) = line?;
        let source = document.line_source(head);
        for _ in 0..head.repeat {
            if !current() {
                return Err(interrupted());
            }
            if !first {
                output.write_all(b"\n")?;
            }
            first = false;
            let mut position = 0;
            while position < source.len() {
                if !current() {
                    return Err(interrupted());
                }
                let count = source.read_at(position, &mut buffer)?;
                if count == 0 {
                    return Err(invalid("retained copy source disappeared"));
                }
                output.write_all(&buffer[..count])?;
                position += count as u64;
            }
        }
    }
    Ok(())
}

fn paint(document: &Document<'_>, row: Row, scratch: &mut Vec<u8>) -> io::Result<Line<'static>> {
    let (_, head) = document.line(row.line)?;
    let source = document.line_source(head);
    let mut glyphs = DiskGlyphs::new(document, head);
    let mut position = row.start;
    let mut barrier = false;
    let mut spans: Vec<Span<'static>> = Vec::new();
    while position < row.end {
        let block = glyphs.block(position)?;
        if block.cells == 0 {
            position = block.end.min(row.end);
            barrier = true;
            continue;
        }
        let end = position
            .checked_add(block.unit)
            .ok_or_else(|| invalid("visible grapheme overflow"))?;
        if end > row.end {
            return Err(invalid("partial visible grapheme"));
        }
        let style = source.style_at(position)?;
        scratch.resize(
            usize::try_from(end - position).map_err(io::Error::other)?,
            0,
        );
        exact(&source, position, scratch)?;
        let text = std::str::from_utf8(scratch).map_err(|_| invalid("invalid visible grapheme"))?;
        if let Some(previous) = spans
            .last_mut()
            .filter(|previous| !barrier && previous.style == style)
        {
            previous.content.to_mut().push_str(text);
        } else {
            spans.push(Span::styled(text.to_owned(), style));
        }
        barrier = false;
        position = end;
    }
    Ok(Line::from(spans))
}
fn interrupted() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "retained viewport superseded")
}

#[cfg(test)]
mod tests;
