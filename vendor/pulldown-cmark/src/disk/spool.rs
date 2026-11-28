use std::cell::RefCell;
use std::io::{self, Write};
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::input::{copy_range, ByteSource, FileSource, RangeSource, TextRef};

const WRITE_BUFFER_BYTES: usize = 8 * 1024;
const CACHED_WRITERS: usize = 2;
const COPY_BUFFER_BYTES: usize = 64 * 1024;

#[derive(Debug)]
pub struct Spool {
    source: u64,
    file: FileSource,
    buffers: RefCell<[Vec<u8>; CACHED_WRITERS]>,
    copy_buffer: RefCell<Vec<u8>>,
    cancelled: Arc<AtomicBool>,
}

impl Spool {
    pub fn new(source: u64, cancelled: Arc<AtomicBool>) -> io::Result<Self> {
        if cancelled.load(Ordering::Relaxed) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "parser text cancelled",
            ));
        }
        Ok(Self {
            source,
            file: FileSource::new(tempfile::tempfile()?)?,
            buffers: RefCell::new(std::array::from_fn(|_| Vec::new())),
            copy_buffer: RefCell::new(Vec::new()),
            cancelled,
        })
    }

    pub fn append(&self, bytes: &[u8]) -> io::Result<TextRef> {
        self.check_cancelled()?;
        let start = self.file.append(bytes)?;
        Ok(TextRef {
            source: self.source,
            start,
            len: bytes.len() as u64,
        })
    }

    pub(crate) fn empty_reference(&self) -> TextRef {
        TextRef {
            source: self.source,
            start: 0,
            len: 0,
        }
    }

    pub fn view(&self, reference: TextRef) -> io::Result<RangeSource<'_, Self>> {
        self.check_cancelled()?;
        if reference.source != self.source {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unknown text source",
            ));
        }
        RangeSource::new(self, reference.start, reference.len)
    }

    pub(crate) fn reference(&self, range: Range<u64>) -> io::Result<TextRef> {
        self.check_cancelled()?;
        if range.start > range.end || range.end > self.file.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "text range exceeds its source",
            ));
        }
        Ok(TextRef {
            source: self.source,
            start: range.start,
            len: range.end - range.start,
        })
    }

    /// Copy a payload range using a bounded, reusable buffer.
    pub fn copy_to<W: Write + ?Sized>(
        &self,
        reference: TextRef,
        destination: &mut W,
    ) -> io::Result<u64> {
        let source = self.view(reference)?;
        if reference.len == 0 {
            return Ok(0);
        }
        let mut buffer = self.copy_buffer.borrow_mut();
        if buffer.is_empty() {
            buffer
                .try_reserve_exact(COPY_BUFFER_BYTES)
                .map_err(|error| io::Error::new(io::ErrorKind::OutOfMemory, error))?;
            buffer.resize(COPY_BUFFER_BYTES, 0);
        }
        super::input::copy_range_buffered(&source, 0..reference.len, destination, &mut buffer[..])
    }

    fn check_cancelled(&self) -> io::Result<()> {
        if self.cancelled.load(Ordering::Relaxed) {
            Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "parser text cancelled",
            ))
        } else {
            Ok(())
        }
    }

    pub fn writer(&self) -> Writer<'_> {
        let start = self.file.len();
        let buffer = self
            .buffers
            .borrow_mut()
            .iter_mut()
            .find(|buffer| buffer.capacity() != 0)
            .map(std::mem::take)
            .unwrap_or_default();
        Writer {
            spool: self,
            start,
            end: start,
            buffer,
            failed: false,
        }
    }

    pub fn copy_from<S: ByteSource + ?Sized>(
        &self,
        source: &S,
        range: Range<u64>,
    ) -> io::Result<TextRef> {
        self.with_speculation(|| {
            let mut writer = self.writer();
            copy_range(source, range, &mut writer)?;
            writer.finish().map(Some)
        })?
        .ok_or_else(|| io::Error::other("text copy produced no result"))
    }

    pub fn with_speculation<T>(
        &self,
        operation: impl FnOnce() -> io::Result<Option<T>>,
    ) -> io::Result<Option<T>> {
        self.check_cancelled()?;
        let mark = self.file.len();
        let result = operation();
        if !matches!(&result, Ok(Some(_))) && self.file.len() != mark {
            self.file.truncate(mark)?;
        }
        result
    }
}

impl ByteSource for Spool {
    fn len(&self) -> u64 {
        self.file.len()
    }

    fn read_at(&self, offset: u64, destination: &mut [u8]) -> io::Result<usize> {
        self.check_cancelled()?;
        self.file.read_at(offset, destination)
    }
}

#[derive(Debug)]
pub struct Writer<'a> {
    spool: &'a Spool,
    start: u64,
    end: u64,
    buffer: Vec<u8>,
    failed: bool,
}

impl Writer<'_> {
    fn check(&mut self) -> io::Result<()> {
        if self.failed || self.spool.len() != self.end - self.buffer.len() as u64 {
            self.failed = true;
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "interleaved text writers",
            ));
        }
        if let Err(error) = self.spool.check_cancelled() {
            self.failed = true;
            return Err(error);
        }
        Ok(())
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.start == self.end
    }

    pub fn finish(mut self) -> io::Result<TextRef> {
        self.flush()?;
        Ok(TextRef {
            source: self.spool.source,
            start: self.start,
            len: self.end - self.start,
        })
    }
}

impl Write for Writer<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.check()?;
        if bytes.is_empty() {
            return Ok(0);
        }
        if self.buffer.is_empty() && bytes.len() >= WRITE_BUFFER_BYTES {
            return match self.spool.append(bytes) {
                Ok(reference) => {
                    self.end = reference.start + reference.len;
                    Ok(bytes.len())
                }
                Err(error) => {
                    self.failed = true;
                    Err(error)
                }
            };
        }
        if self.buffer.capacity() < WRITE_BUFFER_BYTES {
            self.buffer
                .try_reserve_exact(WRITE_BUFFER_BYTES)
                .map_err(|error| io::Error::new(io::ErrorKind::OutOfMemory, error))?;
        }
        let count = bytes.len().min(WRITE_BUFFER_BYTES - self.buffer.len());
        let end = self
            .end
            .checked_add(count as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "text length overflow"))?;
        self.buffer.extend_from_slice(&bytes[..count]);
        self.end = end;
        if self.buffer.len() == WRITE_BUFFER_BYTES {
            self.flush()?;
        }
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.check()?;
        if self.buffer.is_empty() {
            return Ok(());
        }
        match self.spool.append(&self.buffer) {
            Ok(_) => {
                self.buffer.clear();
                Ok(())
            }
            Err(error) => {
                self.failed = true;
                Err(error)
            }
        }
    }
}

impl Drop for Writer<'_> {
    fn drop(&mut self) {
        self.buffer.clear();
        if let Some(slot) = self
            .spool
            .buffers
            .borrow_mut()
            .iter_mut()
            .find(|buffer| buffer.capacity() == 0)
        {
            *slot = std::mem::take(&mut self.buffer);
        }
    }
}
