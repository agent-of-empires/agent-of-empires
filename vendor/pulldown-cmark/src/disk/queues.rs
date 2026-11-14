use std::io;
use std::marker::PhantomData;

use crate::storage::{QueueCursor, Queues};

use super::{Record, Store};

pub(crate) trait NumericKey: Copy {
    fn word(self) -> u64;
}

impl NumericKey for usize {
    fn word(self) -> u64 {
        self as u64
    }
}

impl NumericKey for u8 {
    fn word(self) -> u64 {
        u64::from(self)
    }
}

fn encoded_key(key: impl NumericKey) -> [u8; 32] {
    let mut bytes = [0; 32];
    bytes[..8].copy_from_slice(&key.word().to_le_bytes());
    bytes
}

pub(crate) struct DiskQueues<'a, K, T> {
    store: &'a Store,
    collection: i64,
    marker: PhantomData<fn() -> (K, T)>,
}

impl<'a, K, T> DiskQueues<'a, K, T> {
    pub(crate) fn new(store: &'a Store) -> io::Result<Self> {
        Ok(Self {
            store,
            collection: store.collection()?,
            marker: PhantomData,
        })
    }
}

impl<K: NumericKey, T: Record + Copy> Queues<K, T> for DiskQueues<'_, K, T> {
    type Error = io::Error;
    type Cursor<'a>
        = DiskQueueCursor<'a, T>
    where
        Self: 'a,
        T: 'a;

    fn cursor(&mut self, key: K) -> io::Result<Self::Cursor<'_>> {
        Ok(DiskQueueCursor {
            store: self.store,
            collection: self.collection,
            key: encoded_key(key),
            front: None,
        })
    }

    fn push_back(&mut self, key: K, value: T) -> io::Result<()> {
        self.store
            .insert(self.collection, &encoded_key(key), &value)
            .map(|_| ())
    }

    fn clear(&mut self) -> io::Result<()> {
        self.store.clear_entries(self.collection)
    }
}

pub(crate) struct DiskQueueCursor<'a, T> {
    store: &'a Store,
    collection: i64,
    key: [u8; 32],
    front: Option<(u64, T)>,
}

impl<T: Record + Copy> QueueCursor<T> for DiskQueueCursor<'_, T> {
    type Error = io::Error;

    fn front(&mut self) -> io::Result<Option<T>> {
        if self.front.is_none() {
            self.front = self.store.lookup(self.collection, &self.key, 0)?;
        }
        Ok(self.front.map(|(_, value)| value))
    }

    fn pop_front(&mut self) -> io::Result<()> {
        let entry = self
            .front
            .as_ref()
            .map(|(entry, _)| *entry)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "parser queue cursor has no front")
            })?;
        if !self.store.remove(entry)? {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "parser queue front disappeared",
            ));
        }
        self.front = None;
        Ok(())
    }

    fn replace_front(&mut self, value: T) -> io::Result<()> {
        let entry = self
            .front
            .as_ref()
            .map(|(entry, _)| *entry)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "parser queue cursor has no front")
            })?;
        self.store.replace(entry, &value)?;
        self.front = Some((entry, value));
        Ok(())
    }
}
