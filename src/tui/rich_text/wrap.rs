use std::{io, ops::Range};

use ratatui::{buffer::CellWidth, text::Line};
use unicode_segmentation::UnicodeSegmentation;

#[derive(Clone, Copy)]
pub(crate) struct Block {
    pub start: u64,
    pub end: u64,
    pub unit: u64,
    pub cells: u64,
    pub ascii: bool,
}
impl Block {
    fn check(&self, position: u64, end: u64) -> io::Result<()> {
        if self.start > position
            || position >= self.end
            || self.end > end
            || (self.cells != 0
                && (self.unit == 0
                    || !(position - self.start).is_multiple_of(self.unit)
                    || !(self.end - self.start).is_multiple_of(self.unit)))
            || (self.ascii && (self.unit != 1 || self.cells != 1))
        {
            return Err(invalid("invalid glyph block"));
        }
        Ok(())
    }
}
pub(crate) struct Metrics {
    pub cells: u64,
    pub last_space: Option<u64>,
}
pub(crate) trait Glyphs {
    fn len(&self) -> u64;
    fn cells(&self) -> u64;
    fn block(&mut self, position: u64) -> io::Result<Block>;
    fn last_space(&mut self, range: Range<u64>) -> io::Result<Option<u64>>;
    fn remaining(&mut self, mut start: u64, end: u64) -> io::Result<Metrics> {
        let mut cells = 0u64;
        let mut last_space = None;
        while start < end {
            let block = self.block(start)?;
            block.check(start, self.len())?;
            let next = end.min(block.end);
            if block.cells != 0 {
                if !(next - start).is_multiple_of(block.unit) {
                    return Err(invalid("unaligned glyph range"));
                }
                cells = cells
                    .checked_add((next - start) / block.unit * block.cells)
                    .ok_or_else(|| invalid("line width overflow"))?;
                if block.ascii {
                    if let Some(space) = self.last_space(start..next)? {
                        last_space = Some(space);
                    }
                }
            }
            start = next;
        }
        Ok(Metrics { cells, last_space })
    }
}

pub(crate) struct Breaks<P> {
    glyphs: P,
    width: u64,
    position: u64,
    start: u64,
    end: u64,
    cells: u64,
    last_space: Option<u64>,
    fast: bool,
    done: bool,
}
impl<P: Glyphs> Breaks<P> {
    pub fn new(glyphs: P, width: u16) -> Self {
        let width = u64::from(width.max(1));
        let fast = glyphs.cells() <= width;
        Self {
            glyphs,
            width,
            position: 0,
            start: 0,
            end: 0,
            cells: 0,
            last_space: None,
            fast,
            done: false,
        }
    }
    fn row(&mut self) -> io::Result<Option<Range<u64>>> {
        if self.fast {
            self.done = true;
            return Ok(Some(0..self.glyphs.len()));
        }
        loop {
            if self.position == self.glyphs.len() {
                self.done = true;
                return Ok((self.start != self.end).then_some(self.start..self.end));
            }
            let block = self.glyphs.block(self.position)?;
            block.check(self.position, self.glyphs.len())?;
            if block.cells == 0 {
                self.position = block.end;
                self.end = block.end;
                continue;
            }
            if self.cells + block.cells > self.width && self.cells != 0 {
                if let Some(space) = self.last_space {
                    let emitted = self.start..space;
                    self.start = space + 1;
                    let remaining = self.glyphs.remaining(self.start, self.end)?;
                    self.cells = remaining.cells;
                    self.last_space = remaining.last_space;
                    if !emitted.is_empty() {
                        return Ok(Some(emitted));
                    }
                    continue;
                }
                let emitted = self.start..self.end;
                self.start = self.end;
                self.cells = 0;
                self.last_space = None;
                return Ok(Some(emitted));
            }
            let available = (block.end - self.position) / block.unit;
            let count = if block.cells > self.width {
                1
            } else {
                ((self.width - self.cells) / block.cells)
                    .max(1)
                    .min(available)
            };
            let next = self.position + count * block.unit;
            if block.ascii {
                if let Some(space) = self.glyphs.last_space(self.position..next)? {
                    self.last_space = Some(space);
                }
            }
            self.cells += count * block.cells;
            self.end = next;
            self.position = next;
            if block.cells > self.width {
                let emitted = self.start..self.end;
                self.start = self.end;
                self.cells = 0;
                self.last_space = None;
                return Ok(Some(emitted));
            }
        }
    }
}
impl<P: Glyphs> Iterator for Breaks<P> {
    type Item = io::Result<Range<u64>>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        match self.row() {
            Ok(Some(range)) => Some(Ok(range)),
            Ok(None) => None,
            Err(error) => {
                self.done = true;
                Some(Err(error))
            }
        }
    }
}

pub(crate) struct MemoryGlyphs<'a> {
    line: &'a Line<'static>,
    length: u64,
    cells: u64,
    span: usize,
    base: u64,
    cached: Option<Block>,
}
impl<'a> MemoryGlyphs<'a> {
    pub fn new(line: &'a Line<'static>, cells: u64) -> Self {
        Self {
            line,
            length: line
                .spans
                .iter()
                .map(|span| span.content.len() as u64)
                .sum(),
            cells,
            span: 0,
            base: 0,
            cached: None,
        }
    }
    fn locate(&mut self, position: u64) -> io::Result<usize> {
        if position >= self.length {
            return Err(invalid("unknown in-memory glyph position"));
        }
        while position < self.base {
            self.span = self
                .span
                .checked_sub(1)
                .ok_or_else(|| invalid("invalid in-memory span position"))?;
            self.base -= self.line.spans[self.span].content.len() as u64;
        }
        while self.span < self.line.spans.len()
            && position >= self.base + self.line.spans[self.span].content.len() as u64
        {
            self.base += self.line.spans[self.span].content.len() as u64;
            self.span += 1;
        }
        if self.span == self.line.spans.len() {
            return Err(invalid("in-memory span disappeared"));
        }
        Ok((position - self.base) as usize)
    }
}
impl Glyphs for MemoryGlyphs<'_> {
    fn len(&self) -> u64 {
        self.length
    }
    fn cells(&self) -> u64 {
        self.cells
    }
    fn block(&mut self, position: u64) -> io::Result<Block> {
        if let Some(block) = self.cached {
            if position >= block.start && position < block.end {
                return Ok(block);
            }
        }
        let local = self.locate(position)?;
        let text = &self.line.spans[self.span].content[local..];
        let mut ascii = text
            .as_bytes()
            .iter()
            .take_while(|byte| byte.is_ascii() && !byte.is_ascii_control())
            .count();
        if text
            .as_bytes()
            .get(ascii)
            .is_some_and(|byte| !byte.is_ascii())
        {
            ascii = ascii.saturating_sub(1);
        }
        let block = if ascii != 0 {
            Block {
                start: position,
                end: position + ascii as u64,
                unit: 1,
                cells: 1,
                ascii: true,
            }
        } else {
            let grapheme = text
                .graphemes(true)
                .next()
                .ok_or_else(|| invalid("in-memory grapheme disappeared"))?;
            let cells = if grapheme.contains(char::is_control) {
                0
            } else {
                u64::from(grapheme.cell_width())
            };
            Block {
                start: position,
                end: position + grapheme.len() as u64,
                unit: grapheme.len() as u64,
                cells,
                ascii: grapheme == " ",
            }
        };
        self.cached = Some(block);
        Ok(block)
    }
    fn last_space(&mut self, range: Range<u64>) -> io::Result<Option<u64>> {
        if range.is_empty() {
            return Ok(None);
        }
        let local = self.locate(range.start)?;
        let length = (range.end - range.start) as usize;
        Ok(
            self.line.spans[self.span].content.as_bytes()[local..local + length]
                .iter()
                .rposition(|byte| *byte == b' ')
                .map(|index| range.start + index as u64),
        )
    }
}
fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
