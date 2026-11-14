use std::io;
use std::num::NonZeroUsize;

use crate::disk::Record;

use super::{Node, TreeIndex};

impl Record for TreeIndex {
    const SIZE: usize = 8;

    fn encode(&self, bytes: &mut [u8]) {
        (self.get() as u64).encode(bytes);
    }

    fn decode(bytes: &[u8]) -> io::Result<Self> {
        let value = usize::try_from(u64::decode(bytes)?)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        NonZeroUsize::new(value)
            .map(TreeIndex)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "zero tree index"))
    }
}

impl<T: Record> Record for Node<T> {
    const SIZE: usize = 16 + T::SIZE;

    fn encode(&self, bytes: &mut [u8]) {
        self.child
            .map_or(0, |index| index.get() as u64)
            .encode(&mut bytes[..8]);
        self.next
            .map_or(0, |index| index.get() as u64)
            .encode(&mut bytes[8..16]);
        self.item.encode(&mut bytes[16..]);
    }

    fn decode(bytes: &[u8]) -> io::Result<Self> {
        Ok(Self {
            child: optional_index(&bytes[..8])?,
            next: optional_index(&bytes[8..16])?,
            item: T::decode(&bytes[16..])?,
        })
    }
}

fn optional_index(bytes: &[u8]) -> io::Result<Option<TreeIndex>> {
    if u64::decode(bytes)? == 0 {
        Ok(None)
    } else {
        TreeIndex::decode(bytes).map(Some)
    }
}
