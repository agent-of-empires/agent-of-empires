use std::convert::Infallible;
use std::ops::{Bound, Range, RangeBounds};

#[cfg(feature = "disk")]
use crate::disk::ByteSource as _;

pub(crate) trait Bytes {
    type Error;

    fn source_len(&self) -> usize;
    fn byte(&self, position: usize) -> Result<Option<u8>, Self::Error>;
    fn find_byte(&self, range: Range<usize>, byte: u8) -> Result<Option<usize>, Self::Error>;
    #[cfg(all(target_arch = "x86_64", feature = "simd"))]
    fn contiguous(&self) -> Option<&[u8]> {
        None
    }
}

pub(crate) trait Source: Bytes {
    fn next_char(&self, start: usize, end: usize) -> Result<Option<char>, Self::Error>;
    fn previous_char(&self, start: usize, end: usize) -> Result<Option<char>, Self::Error>;
}

impl Bytes for [u8] {
    type Error = Infallible;

    fn source_len(&self) -> usize {
        self.len()
    }

    fn byte(&self, position: usize) -> Result<Option<u8>, Infallible> {
        Ok(self.get(position).copied())
    }

    fn find_byte(&self, range: Range<usize>, byte: u8) -> Result<Option<usize>, Infallible> {
        let start = range.start;
        Ok(memchr::memchr(byte, &self[range]).map(|index| start + index))
    }

    #[cfg(all(target_arch = "x86_64", feature = "simd"))]
    fn contiguous(&self) -> Option<&[u8]> {
        Some(self)
    }
}

impl Bytes for str {
    type Error = Infallible;

    fn source_len(&self) -> usize {
        self.len()
    }

    fn byte(&self, position: usize) -> Result<Option<u8>, Infallible> {
        Bytes::byte(self.as_bytes(), position)
    }

    fn find_byte(&self, range: Range<usize>, byte: u8) -> Result<Option<usize>, Infallible> {
        Bytes::find_byte(self.as_bytes(), range, byte)
    }

    #[cfg(all(target_arch = "x86_64", feature = "simd"))]
    fn contiguous(&self) -> Option<&[u8]> {
        Some(self.as_bytes())
    }
}

impl Source for str {
    fn next_char(&self, start: usize, end: usize) -> Result<Option<char>, Infallible> {
        Ok(self[start..end].chars().next())
    }

    fn previous_char(&self, start: usize, end: usize) -> Result<Option<char>, Infallible> {
        Ok(self[start..end].chars().next_back())
    }
}

#[cfg(feature = "disk")]
impl Bytes for crate::disk::Spool {
    type Error = std::io::Error;

    fn source_len(&self) -> usize {
        self.len() as usize
    }

    fn byte(&self, position: usize) -> std::io::Result<Option<u8>> {
        self.byte_at(position as u64)
    }

    fn find_byte(&self, range: Range<usize>, byte: u8) -> std::io::Result<Option<usize>> {
        let mut position = range.start;
        let mut buffer = [0; 8192];
        while position < range.end {
            let wanted = (range.end - position).min(buffer.len());
            let count = self.read_at(position as u64, &mut buffer[..wanted])?;
            if count == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "source range disappeared",
                ));
            }
            if let Some(index) = memchr::memchr(byte, &buffer[..count]) {
                return Ok(Some(position + index));
            }
            position += count;
        }
        Ok(None)
    }
}

#[cfg(feature = "disk")]
impl Source for crate::disk::Spool {
    fn next_char(&self, start: usize, end: usize) -> std::io::Result<Option<char>> {
        crate::disk::Cursor::new(self, start as u64..end as u64)?.next_char()
    }

    fn previous_char(&self, start: usize, end: usize) -> std::io::Result<Option<char>> {
        let mut cursor = crate::disk::Cursor::new(self, start as u64..end as u64)?;
        cursor.seek(end as u64)?;
        cursor.previous_char()
    }
}

pub(crate) fn binary_search_by<T, E>(
    values: &[T],
    mut compare: impl FnMut(&T) -> Result<std::cmp::Ordering, E>,
) -> Result<Option<usize>, E> {
    use std::cmp::Ordering;

    let mut left = 0;
    let mut right = values.len();
    while left < right {
        let middle = left + (right - left) / 2;
        match compare(&values[middle])? {
            Ordering::Less => left = middle + 1,
            Ordering::Greater => right = middle,
            Ordering::Equal => return Ok(Some(middle)),
        }
    }
    Ok(None)
}

pub(crate) struct Text<'a, S: Bytes + ?Sized> {
    pub(crate) source: &'a S,
    pub(crate) start: usize,
    pub(crate) end: usize,
}

impl<S: Bytes + ?Sized> Copy for Text<'_, S> {}

impl<S: Bytes + ?Sized> Clone for Text<'_, S> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<S: Bytes + ?Sized> std::fmt::Debug for Text<'_, S> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Text")
            .field("start", &self.start)
            .field("end", &self.end)
            .finish()
    }
}

impl<'a, S: Bytes + ?Sized> Text<'a, S> {
    pub(crate) fn new(source: &'a S) -> Self {
        Self {
            source,
            start: 0,
            end: source.source_len(),
        }
    }

    pub(crate) fn len(self) -> usize {
        self.end - self.start
    }

    pub(crate) fn is_empty(self) -> bool {
        self.start == self.end
    }

    pub(crate) fn slice(self, range: impl RangeBounds<usize>) -> Self {
        let start = match range.start_bound() {
            Bound::Included(index) => *index,
            Bound::Excluded(index) => index.checked_add(1).expect("text offset overflow"),
            Bound::Unbounded => 0,
        };
        let end = match range.end_bound() {
            Bound::Included(index) => index.checked_add(1).expect("text offset overflow"),
            Bound::Excluded(index) => *index,
            Bound::Unbounded => self.len(),
        };
        assert!(
            start <= end && end <= self.len(),
            "text slice exceeds its range"
        );
        Self {
            source: self.source,
            start: self.start + start,
            end: self.start + end,
        }
    }

    pub(crate) fn get(self, position: usize) -> Result<Option<u8>, S::Error> {
        if position >= self.len() {
            Ok(None)
        } else {
            self.source.byte(self.start + position)
        }
    }

    pub(crate) fn byte(self, position: usize) -> Result<u8, S::Error> {
        assert!(position < self.len(), "byte offset exceeds its range");
        self.source
            .byte(self.start + position)
            .map(|byte| byte.expect("source length changed"))
    }

    pub(crate) fn find_byte(self, byte: u8) -> Result<Option<usize>, S::Error> {
        self.source
            .find_byte(self.start..self.end, byte)
            .map(|position| position.map(|position| position - self.start))
    }

    pub(crate) fn starts_with(self, prefix: &[u8]) -> Result<bool, S::Error> {
        if prefix.len() > self.len() {
            return Ok(false);
        }
        for (index, expected) in prefix.iter().enumerate() {
            if self.byte(index)? != *expected {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub(crate) fn ends_with(self, suffix: &[u8]) -> Result<bool, S::Error> {
        if suffix.len() > self.len() {
            return Ok(false);
        }
        self.slice(self.len() - suffix.len()..).starts_with(suffix)
    }

    pub(crate) fn contains(self, needle: &[u8]) -> Result<bool, S::Error> {
        let Some(first) = needle.first() else {
            return Ok(true);
        };
        let mut start = 0;
        while let Some(offset) = self.slice(start..).find_byte(*first)? {
            start += offset;
            if self.slice(start..).starts_with(needle)? {
                return Ok(true);
            }
            start += 1;
        }
        Ok(false)
    }

    #[cfg(all(target_arch = "x86_64", feature = "simd"))]
    pub(crate) fn contiguous(self) -> Option<&'a [u8]> {
        self.source
            .contiguous()
            .map(|bytes| &bytes[self.start..self.end])
    }

    pub(crate) fn cmp_bytes(self, other: &[u8]) -> Result<std::cmp::Ordering, S::Error> {
        for (index, byte) in other.iter().take(self.len()).enumerate() {
            let order = self.byte(index)?.cmp(byte);
            if !order.is_eq() {
                return Ok(order);
            }
        }
        Ok(self.len().cmp(&other.len()))
    }
}

impl<'a, S: Source + ?Sized> Text<'a, S> {
    pub(crate) fn first_char(self) -> Result<Option<char>, S::Error> {
        self.source.next_char(self.start, self.end)
    }

    pub(crate) fn last_char(self) -> Result<Option<char>, S::Error> {
        self.source.previous_char(self.start, self.end)
    }

    pub(crate) fn trim(self, predicate: impl Fn(char) -> bool) -> Result<Self, S::Error> {
        let mut start = self.start;
        let mut end = self.end;
        while let Some(value) = self.source.next_char(start, end)? {
            if !predicate(value) {
                break;
            }
            start += value.len_utf8();
        }
        while let Some(value) = self.source.previous_char(start, end)? {
            if !predicate(value) {
                break;
            }
            end -= value.len_utf8();
        }
        Ok(Self {
            source: self.source,
            start,
            end,
        })
    }
}

impl<'a> Text<'a, str> {
    pub(crate) fn as_str(self) -> &'a str {
        &self.source[self.start..self.end]
    }
}
