use std::cell::{Cell, RefCell};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::ops::Range;

const PAGE_BYTES: usize = 64 * 1024;
const CACHE_PAGES: usize = 4;

pub trait ByteSource {
    fn len(&self) -> u64;
    fn read_at(&self, offset: u64, destination: &mut [u8]) -> io::Result<usize>;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn byte_at(&self, offset: u64) -> io::Result<Option<u8>> {
        if offset == self.len() {
            return Ok(None);
        }
        if offset > self.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "source offset exceeds its length",
            ));
        }
        let mut byte = [0];
        match self.read_at(offset, &mut byte)? {
            0 => Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "source byte disappeared",
            )),
            _ => Ok(Some(byte[0])),
        }
    }
}

impl ByteSource for str {
    fn len(&self) -> u64 {
        str::len(self) as u64
    }

    fn read_at(&self, offset: u64, destination: &mut [u8]) -> io::Result<usize> {
        let offset = usize::try_from(offset)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "source offset overflow"))?;
        let bytes = self.as_bytes().get(offset..).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "source offset exceeds its length",
            )
        })?;
        let count = destination.len().min(bytes.len());
        destination[..count].copy_from_slice(&bytes[..count]);
        Ok(count)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RangeSource<'a, S: ?Sized> {
    source: &'a S,
    start: u64,
    length: u64,
}

impl<'a, S: ByteSource + ?Sized> RangeSource<'a, S> {
    pub fn new(source: &'a S, start: u64, length: u64) -> io::Result<Self> {
        let end = start
            .checked_add(length)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "source range overflow"))?;
        if end > source.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "source range exceeds its length",
            ));
        }
        Ok(Self {
            source,
            start,
            length,
        })
    }
}

impl<S: ByteSource + ?Sized> ByteSource for RangeSource<'_, S> {
    fn len(&self) -> u64 {
        self.length
    }

    fn read_at(&self, offset: u64, destination: &mut [u8]) -> io::Result<usize> {
        let remaining = self.length.checked_sub(offset).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "source offset exceeds its range",
            )
        })?;
        let count = remaining.min(destination.len() as u64) as usize;
        if count == 0 {
            return Ok(0);
        }
        let count = self
            .source
            .read_at(self.start + offset, &mut destination[..count])?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "source range disappeared",
            ));
        }
        Ok(count)
    }
}

#[derive(Debug)]
struct Page {
    index: Option<u64>,
    valid: usize,
    bytes: Box<[u8]>,
}

#[derive(Debug)]
struct CachedFile {
    file: File,
    pages: [Option<Page>; CACHE_PAGES],
}

#[derive(Debug)]
pub struct FileSource {
    length: Cell<u64>,
    inner: RefCell<CachedFile>,
}

impl FileSource {
    pub fn new(file: File) -> io::Result<Self> {
        let length = file.metadata()?.len();
        Ok(Self {
            length: Cell::new(length),
            inner: RefCell::new(CachedFile {
                file,
                pages: std::array::from_fn(|_| None),
            }),
        })
    }

    pub fn into_file(self) -> File {
        self.inner.into_inner().file
    }

    pub(super) fn append(&self, bytes: &[u8]) -> io::Result<u64> {
        let start = self.length.get();
        if bytes.is_empty() {
            return Ok(start);
        }
        let end = start
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "spool length overflow"))?;
        let mut inner = self.inner.borrow_mut();
        inner.file.seek(SeekFrom::Start(start))?;
        if let Err(error) = inner.file.write_all(bytes) {
            inner.file.set_len(start)?;
            return Err(error);
        }
        self.length.set(end);
        for page in inner.pages.iter_mut().flatten() {
            if page
                .index
                .is_some_and(|index| index >= start / PAGE_BYTES as u64)
            {
                page.index = None;
                page.valid = 0;
            }
        }
        Ok(start)
    }

    pub(super) fn truncate(&self, length: u64) -> io::Result<()> {
        if length > self.length.get() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "spool rollback exceeds its length",
            ));
        }
        let mut inner = self.inner.borrow_mut();
        inner.file.set_len(length)?;
        self.length.set(length);
        for page in inner.pages.iter_mut().flatten() {
            if page
                .index
                .is_some_and(|index| index >= length / PAGE_BYTES as u64)
            {
                page.index = None;
                page.valid = 0;
            }
        }
        Ok(())
    }
}

impl ByteSource for FileSource {
    fn len(&self) -> u64 {
        self.length.get()
    }

    fn read_at(&self, offset: u64, destination: &mut [u8]) -> io::Result<usize> {
        let length = self.length.get();
        if offset > length {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "source offset exceeds its length",
            ));
        }
        let count = usize::try_from((length - offset).min(destination.len() as u64))
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "source range overflow"))?;
        let mut copied = 0;
        let mut inner = self.inner.borrow_mut();
        let CachedFile { file, pages } = &mut *inner;
        while copied < count {
            let position = offset + copied as u64;
            let index = position / PAGE_BYTES as u64;
            let slot = (index % CACHE_PAGES as u64) as usize;
            let page = pages[slot].get_or_insert_with(|| Page {
                index: None,
                valid: 0,
                bytes: vec![0; PAGE_BYTES].into_boxed_slice(),
            });
            if page.index != Some(index) {
                page.index = None;
                page.valid = 0;
                let start = index * PAGE_BYTES as u64;
                let valid = (length - start).min(PAGE_BYTES as u64) as usize;
                file.seek(SeekFrom::Start(start))?;
                file.read_exact(&mut page.bytes[..valid])?;
                page.valid = valid;
                page.index = Some(index);
            }
            let within = (position % PAGE_BYTES as u64) as usize;
            let available = (page.valid - within).min(count - copied);
            destination[copied..copied + available]
                .copy_from_slice(&page.bytes[within..within + available]);
            copied += available;
        }
        Ok(copied)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextRef {
    pub source: u64,
    pub start: u64,
    pub len: u64,
}

impl TextRef {
    pub fn subrange(self, range: Range<u64>) -> io::Result<Self> {
        if range.start > range.end || range.end > self.len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid text range",
            ));
        }
        Ok(Self {
            source: self.source,
            start: self.start.checked_add(range.start).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "text range overflow")
            })?,
            len: range.end - range.start,
        })
    }
}

impl super::Record for TextRef {
    const SIZE: usize = 24;

    fn encode(&self, bytes: &mut [u8]) {
        bytes[..8].copy_from_slice(&self.source.to_le_bytes());
        bytes[8..16].copy_from_slice(&self.start.to_le_bytes());
        bytes[16..24].copy_from_slice(&self.len.to_le_bytes());
    }

    fn decode(bytes: &[u8]) -> io::Result<Self> {
        let bytes: &[u8; Self::SIZE] = bytes
            .try_into()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let source = u64::from_le_bytes(bytes[..8].try_into().map_err(io::Error::other)?);
        let start = u64::from_le_bytes(bytes[8..16].try_into().map_err(io::Error::other)?);
        let len = u64::from_le_bytes(bytes[16..24].try_into().map_err(io::Error::other)?);
        if start.checked_add(len).is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "text reference overflow",
            ));
        }
        Ok(Self { source, start, len })
    }
}

pub fn copy_range<S: ByteSource + ?Sized, W: Write + ?Sized>(
    source: &S,
    range: Range<u64>,
    destination: &mut W,
) -> io::Result<u64> {
    let mut buffer = [0; PAGE_BYTES];
    copy_range_buffered(source, range, destination, &mut buffer)
}

pub(super) fn copy_range_buffered<S: ByteSource + ?Sized, W: Write + ?Sized>(
    source: &S,
    range: Range<u64>,
    destination: &mut W,
    buffer: &mut [u8],
) -> io::Result<u64> {
    if range.start > range.end || range.end > source.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid source range",
        ));
    }
    let mut position = range.start;
    while position < range.end {
        let wanted = (range.end - position).min(buffer.len() as u64) as usize;
        let count = source.read_at(position, &mut buffer[..wanted])?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "source range disappeared",
            ));
        }
        destination.write_all(&buffer[..count])?;
        position += count as u64;
    }
    Ok(range.end - range.start)
}

#[derive(Debug)]
pub struct Cursor<'a, S: ByteSource + ?Sized> {
    source: &'a S,
    start: u64,
    end: u64,
    position: u64,
}

impl<'a, S: ByteSource + ?Sized> Cursor<'a, S> {
    pub fn new(source: &'a S, range: Range<u64>) -> io::Result<Self> {
        if range.start > range.end || range.end > source.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid cursor range",
            ));
        }
        Ok(Self {
            source,
            start: range.start,
            end: range.end,
            position: range.start,
        })
    }

    pub fn position(&self) -> u64 {
        self.position
    }

    pub fn remaining(&self) -> u64 {
        self.end - self.position
    }

    pub fn seek(&mut self, position: u64) -> io::Result<()> {
        if position < self.start || position > self.end {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cursor seek exceeds its range",
            ));
        }
        self.position = position;
        Ok(())
    }

    pub fn peek(&self) -> io::Result<Option<u8>> {
        if self.position == self.end {
            Ok(None)
        } else {
            self.source.byte_at(self.position)
        }
    }

    pub fn next_byte(&mut self) -> io::Result<Option<u8>> {
        let byte = self.peek()?;
        if byte.is_some() {
            self.position += 1;
        }
        Ok(byte)
    }

    pub fn next_char(&mut self) -> io::Result<Option<char>> {
        let Some(first) = self.peek()? else {
            return Ok(None);
        };
        let width = match first {
            0..=0x7f => 1,
            0xc2..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf4 => 4,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid UTF-8 prefix",
                ))
            }
        };
        if self.end - self.position < width as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "incomplete UTF-8 scalar",
            ));
        }
        let mut bytes = [0; 4];
        let mut copied = 0;
        while copied < width {
            let count = self
                .source
                .read_at(self.position + copied as u64, &mut bytes[copied..width])?;
            if count == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "UTF-8 scalar disappeared",
                ));
            }
            copied += count;
        }
        let text = std::str::from_utf8(&bytes[..width])
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let value = text
            .chars()
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "empty UTF-8 scalar"))?;
        self.position += width as u64;
        Ok(Some(value))
    }

    pub fn previous_char(&mut self) -> io::Result<Option<char>> {
        if self.position == self.start {
            return Ok(None);
        }
        let end = self.position;
        let mut start = end - 1;
        let mut continuation = 0;
        while self
            .source
            .byte_at(start)?
            .is_some_and(|byte| byte & 0xc0 == 0x80)
        {
            continuation += 1;
            if continuation > 3 || start == self.start {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid UTF-8 suffix",
                ));
            }
            start -= 1;
        }
        self.position = start;
        let value = self.next_char()?;
        if self.position != end {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "UTF-8 boundary mismatch",
            ));
        }
        self.position = start;
        Ok(value)
    }
}
