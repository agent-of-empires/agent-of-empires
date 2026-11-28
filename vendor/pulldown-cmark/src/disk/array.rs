use std::io;
use std::marker::PhantomData;
use std::ops::Range;

use super::{Record, Store};

const ARRAY_BYTES: usize = 24;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArrayRef<T> {
    collection: i64,
    start: u64,
    length: u64,
    marker: PhantomData<fn() -> T>,
}

impl<T> Default for ArrayRef<T> {
    fn default() -> Self {
        Self {
            collection: 0,
            start: 0,
            length: 0,
            marker: PhantomData,
        }
    }
}

impl<T> ArrayRef<T> {
    pub(crate) fn new(collection: i64, start: u64, length: u64) -> io::Result<Self> {
        if collection < 0 || (collection == 0 && length != 0) || start.checked_add(length).is_none()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid parser array range",
            ));
        }
        Ok(Self {
            collection,
            start,
            length,
            marker: PhantomData,
        })
    }

    pub fn len(&self) -> u64 {
        self.length
    }

    pub fn is_empty(&self) -> bool {
        self.length == 0
    }

    pub fn slice(&self, range: Range<u64>) -> io::Result<Self> {
        if range.start > range.end || range.end > self.length {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "parser array slice exceeds its range",
            ));
        }
        Self::new(
            self.collection,
            self.start + range.start,
            range.end - range.start,
        )
    }
}

impl<T: Record> ArrayRef<T> {
    pub fn get(&self, store: &Store, index: u64) -> io::Result<T> {
        if index >= self.length {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "parser array index exceeds its range",
            ));
        }
        store.read(self.collection, self.start + index)
    }

    pub fn iter<'a>(&self, store: &'a Store) -> impl DoubleEndedIterator<Item = io::Result<T>> + 'a
    where
        T: 'a,
    {
        let collection = self.collection;
        let range = self.start..self.start + self.length;
        range.map(move |position| store.read(collection, position))
    }
}

impl<T> Record for ArrayRef<T> {
    const SIZE: usize = ARRAY_BYTES;

    fn encode(&self, bytes: &mut [u8]) {
        bytes[..8].copy_from_slice(&self.collection.to_le_bytes());
        bytes[8..16].copy_from_slice(&self.start.to_le_bytes());
        bytes[16..24].copy_from_slice(&self.length.to_le_bytes());
    }

    fn decode(bytes: &[u8]) -> io::Result<Self> {
        let bytes: &[u8; ARRAY_BYTES] = bytes
            .try_into()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let collection = i64::from_le_bytes(bytes[..8].try_into().map_err(io::Error::other)?);
        let start = u64::from_le_bytes(bytes[8..16].try_into().map_err(io::Error::other)?);
        let length = u64::from_le_bytes(bytes[16..24].try_into().map_err(io::Error::other)?);
        Self::new(collection, start, length)
    }
}
