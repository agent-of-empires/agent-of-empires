use std::cell::Cell;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use rusqlite::{params, Connection, OptionalExtension};
use tempfile::NamedTempFile;

pub trait Record: Sized {
    const SIZE: usize;
    fn encode(&self, bytes: &mut [u8]);
    fn decode(bytes: &[u8]) -> io::Result<Self>;
}

const RECORD_BYTES: usize = 128;

pub struct Store {
    connection: Connection,
    file: NamedTempFile,
    next_collection: Cell<i64>,
    cancelled: Arc<AtomicBool>,
}

impl Store {
    pub fn new(cancelled: Arc<AtomicBool>) -> io::Result<Self> {
        if cancelled.load(Ordering::Relaxed) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "parser storage cancelled",
            ));
        }
        let file = NamedTempFile::new()?;
        let mut connection = Connection::open(file.path()).map_err(io::Error::other)?;
        let progress_cancelled = Arc::clone(&cancelled);
        connection
            .progress_handler(
                1000,
                Some(move || progress_cancelled.load(Ordering::Relaxed)),
            )
            .map_err(io::Error::other)?;
        connection
            .execute_batch(
                "PRAGMA cache_size = -256;
                 PRAGMA mmap_size = 0;
                 PRAGMA temp_store = FILE;
                 PRAGMA journal_mode = DELETE;
                 PRAGMA synchronous = OFF;
                 CREATE TABLE records (
                     collection INTEGER NOT NULL,
                     position INTEGER NOT NULL,
                     value BLOB NOT NULL,
                     PRIMARY KEY (collection, position)
                 ) WITHOUT ROWID;
                 CREATE TABLE entries (
                     id INTEGER PRIMARY KEY,
                     collection INTEGER NOT NULL,
                     key BLOB NOT NULL,
                     value BLOB NOT NULL
                 );
                 CREATE INDEX entry_keys ON entries (collection, key, id);
                 CREATE INDEX entry_order ON entries (collection, id);",
            )
            .map_err(io::Error::other)?;
        connection.set_prepared_statement_cache_capacity(8);
        Ok(Self {
            connection,
            file,
            next_collection: Cell::new(0),
            cancelled,
        })
    }

    pub(crate) fn cancellation(&self) -> &AtomicBool {
        &self.cancelled
    }

    fn check_cancelled(&self) -> io::Result<()> {
        if self.cancelled.load(Ordering::Relaxed) {
            Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "parser storage cancelled",
            ))
        } else {
            Ok(())
        }
    }

    pub fn with_transaction<T>(
        &self,
        operation: impl FnOnce(&Self) -> io::Result<T>,
    ) -> io::Result<T> {
        self.check_cancelled()?;
        let transaction = self
            .connection
            .unchecked_transaction()
            .map_err(io::Error::other)?;
        match operation(self) {
            Ok(value) => {
                transaction.commit().map_err(io::Error::other)?;
                Ok(value)
            }
            Err(error) => {
                transaction.rollback().map_err(io::Error::other)?;
                Err(error)
            }
        }
    }

    pub fn collection(&self) -> io::Result<i64> {
        self.check_cancelled()?;
        let next = self.next_collection.get();
        self.next_collection.set(next.checked_add(1).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "collection identifier overflow")
        })?);
        Ok(next)
    }

    pub fn clear(&mut self) -> io::Result<()> {
        self.check_cancelled()?;
        self.connection
            .execute_batch("DELETE FROM records; DELETE FROM entries;")
            .map_err(io::Error::other)?;
        self.next_collection.set(0);
        Ok(())
    }

    pub fn read<T: Record>(&self, collection: i64, position: u64) -> io::Result<T> {
        self.check_cancelled()?;
        let position = sql_position(position)?;
        let mut statement = self
            .connection
            .prepare_cached("SELECT value FROM records WHERE collection = ?1 AND position = ?2")
            .map_err(io::Error::other)?;
        statement
            .query_row(params![collection, position], |row| {
                let value = row.get_ref(0)?.as_blob()?;
                Ok(decode::<T>(value))
            })
            .map_err(io::Error::other)?
    }

    pub fn write<T: Record>(&self, collection: i64, position: u64, record: &T) -> io::Result<()> {
        self.check_cancelled()?;
        let mut buffer = [0; RECORD_BYTES];
        let bytes = encoded(record, &mut buffer)?;
        let position = sql_position(position)?;
        self.connection
            .prepare_cached(
                "INSERT INTO records (collection, position, value) VALUES (?1, ?2, ?3)
                 ON CONFLICT (collection, position) DO UPDATE SET value = excluded.value",
            )
            .map_err(io::Error::other)?
            .execute(params![collection, position, bytes])
            .map_err(io::Error::other)?;
        Ok(())
    }

    pub fn truncate(&self, collection: i64, length: u64) -> io::Result<()> {
        self.check_cancelled()?;
        self.connection
            .prepare_cached("DELETE FROM records WHERE collection = ?1 AND position >= ?2")
            .map_err(io::Error::other)?
            .execute(params![collection, sql_position(length)?])
            .map_err(io::Error::other)?;
        Ok(())
    }
    /// Remove one occupied record without shifting later positions.
    pub fn remove_record(&self, collection: i64, position: u64) -> io::Result<bool> {
        self.check_cancelled()?;
        self.connection
            .prepare_cached("DELETE FROM records WHERE collection = ?1 AND position = ?2")
            .map_err(io::Error::other)?
            .execute(params![collection, sql_position(position)?])
            .map(|changed| changed != 0)
            .map_err(io::Error::other)
    }

    pub fn lookup<T: Record>(
        &self,
        collection: i64,
        key: &[u8; 32],
        after: u64,
    ) -> io::Result<Option<(u64, T)>> {
        self.check_cancelled()?;
        let mut statement = self
            .connection
            .prepare_cached(
                "SELECT id, value FROM entries
                 WHERE collection = ?1 AND key = ?2 AND id > ?3 ORDER BY id LIMIT 1",
            )
            .map_err(io::Error::other)?;
        statement
            .query_row(params![collection, &key[..], sql_position(after)?], |row| {
                let id: i64 = row.get(0)?;
                let value = row.get_ref(1)?.as_blob()?;
                Ok(decode::<T>(value).map(|value| (id as u64, value)))
            })
            .optional()
            .map_err(io::Error::other)?
            .transpose()
    }

    pub fn insert<T: Record>(
        &self,
        collection: i64,
        key: &[u8; 32],
        record: &T,
    ) -> io::Result<u64> {
        self.check_cancelled()?;
        let mut buffer = [0; RECORD_BYTES];
        let bytes = encoded(record, &mut buffer)?;
        self.connection
            .prepare_cached("INSERT INTO entries (collection, key, value) VALUES (?1, ?2, ?3)")
            .map_err(io::Error::other)?
            .execute(params![collection, &key[..], bytes])
            .map_err(io::Error::other)?;
        Ok(self.connection.last_insert_rowid() as u64)
    }

    pub fn replace<T: Record>(&self, entry: u64, record: &T) -> io::Result<()> {
        self.check_cancelled()?;
        let mut buffer = [0; RECORD_BYTES];
        let bytes = encoded(record, &mut buffer)?;
        let changed = self
            .connection
            .prepare_cached("UPDATE entries SET value = ?1 WHERE id = ?2")
            .map_err(io::Error::other)?
            .execute(params![bytes, sql_position(entry)?])
            .map_err(io::Error::other)?;
        if changed == 0 {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "parser map entry disappeared",
            ));
        }
        Ok(())
    }

    pub fn remove(&self, entry: u64) -> io::Result<bool> {
        self.check_cancelled()?;
        self.connection
            .prepare_cached("DELETE FROM entries WHERE id = ?1")
            .map_err(io::Error::other)?
            .execute(params![sql_position(entry)?])
            .map(|changed| changed != 0)
            .map_err(io::Error::other)
    }

    pub fn clear_entries(&self, collection: i64) -> io::Result<()> {
        self.check_cancelled()?;
        self.connection
            .prepare_cached("DELETE FROM entries WHERE collection = ?1")
            .map_err(io::Error::other)?
            .execute(params![collection])
            .map_err(io::Error::other)?;
        Ok(())
    }

    /// Read the next live map entry in insertion order.
    pub fn next_entry<T: Record>(
        &self,
        collection: i64,
        after: u64,
    ) -> io::Result<Option<(u64, T)>> {
        self.check_cancelled()?;
        self.connection
            .prepare_cached(
                "SELECT id, value FROM entries
                 WHERE collection = ?1 AND id > ?2 ORDER BY id LIMIT 1",
            )
            .map_err(io::Error::other)?
            .query_row(params![collection, sql_position(after)?], |row| {
                let id: i64 = row.get(0)?;
                let bytes = row.get_ref(1)?.as_blob()?;
                Ok(decode::<T>(bytes).map(|value| (id as u64, value)))
            })
            .optional()
            .map_err(io::Error::other)?
            .transpose()
    }

    /// Read the next occupied record position, including sparse collections.
    pub fn next_record<T: Record>(
        &self,
        collection: i64,
        after: Option<u64>,
    ) -> io::Result<Option<(u64, T)>> {
        self.check_cancelled()?;
        let after = after.map(sql_position).transpose()?.unwrap_or(-1);
        self.connection
            .prepare_cached(
                "SELECT position, value FROM records
                 WHERE collection = ?1 AND position > ?2 ORDER BY position LIMIT 1",
            )
            .map_err(io::Error::other)?
            .query_row(params![collection, after], |row| {
                let position: i64 = row.get(0)?;
                let bytes = row.get_ref(1)?.as_blob()?;
                Ok(decode::<T>(bytes).map(|value| (position as u64, value)))
            })
            .optional()
            .map_err(io::Error::other)?
            .transpose()
    }

    /// Read the occupied record at or immediately before a position.
    pub fn floor_record<T: Record>(
        &self,
        collection: i64,
        position: u64,
    ) -> io::Result<Option<(u64, T)>> {
        self.check_cancelled()?;
        self.connection
            .prepare_cached(
                "SELECT position, value FROM records
                 WHERE collection = ?1 AND position <= ?2 ORDER BY position DESC LIMIT 1",
            )
            .map_err(io::Error::other)?
            .query_row(params![collection, sql_position(position)?], |row| {
                let position: i64 = row.get(0)?;
                let bytes = row.get_ref(1)?.as_blob()?;
                Ok(decode::<T>(bytes).map(|value| (position as u64, value)))
            })
            .optional()
            .map_err(io::Error::other)?
            .transpose()
    }

    /// Release an exclusively owned range of temporary collections.
    pub fn clear_collections(&self, start: i64, end: i64) -> io::Result<()> {
        self.check_cancelled()?;
        if start < 0 || end < start {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid collection range",
            ));
        }
        self.with_transaction(|_| {
            self.connection
                .execute(
                    "DELETE FROM records WHERE collection >= ?1 AND collection < ?2",
                    params![start, end],
                )
                .map_err(io::Error::other)?;
            self.connection
                .execute(
                    "DELETE FROM entries WHERE collection >= ?1 AND collection < ?2",
                    params![start, end],
                )
                .map_err(io::Error::other)?;
            Ok(())
        })
    }
}

impl std::fmt::Debug for Store {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Store")
            .field("file", &self.file.path())
            .finish_non_exhaustive()
    }
}

fn sql_position(position: u64) -> io::Result<i64> {
    i64::try_from(position)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "record position overflow"))
}

fn encoded<'a, T: Record>(record: &T, buffer: &'a mut [u8; RECORD_BYTES]) -> io::Result<&'a [u8]> {
    let bytes = buffer.get_mut(..T::SIZE).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "record encoding exceeds its fixed storage",
        )
    })?;
    record.encode(bytes);
    Ok(bytes)
}

fn decode<T: Record>(bytes: &[u8]) -> io::Result<T> {
    if bytes.len() != T::SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid record size",
        ));
    }
    T::decode(bytes)
}

#[derive(Debug)]
pub struct Sequence<'a, T> {
    store: &'a Store,
    collection: i64,
    length: u64,
    marker: std::marker::PhantomData<T>,
}

impl<'a, T: Record> Sequence<'a, T> {
    pub fn new(store: &'a Store) -> io::Result<Self> {
        Ok(Self {
            store,
            collection: store.collection()?,
            length: 0,
            marker: std::marker::PhantomData,
        })
    }

    pub fn len(&self) -> u64 {
        self.length
    }

    pub fn is_empty(&self) -> bool {
        self.length == 0
    }

    pub fn view(&self) -> io::Result<super::ArrayRef<T>> {
        super::ArrayRef::new(self.collection, 0, self.length)
    }

    pub fn get(&self, position: u64) -> io::Result<T> {
        if position >= self.length {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "sequence position exceeds its length",
            ));
        }
        self.store.read(self.collection, position)
    }

    pub fn set(&self, position: u64, record: &T) -> io::Result<()> {
        if position >= self.length {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "sequence position exceeds its length",
            ));
        }
        self.store.write(self.collection, position, record)
    }

    pub fn push(&mut self, record: &T) -> io::Result<()> {
        let next = self.length.checked_add(1).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "sequence length overflow")
        })?;
        self.store.write(self.collection, self.length, record)?;
        self.length = next;
        Ok(())
    }

    pub fn pop(&mut self) -> io::Result<Option<T>> {
        let Some(position) = self.length.checked_sub(1) else {
            return Ok(None);
        };
        let record = self.store.read(self.collection, position)?;
        self.store.truncate(self.collection, position)?;
        self.length = position;
        Ok(Some(record))
    }

    pub fn truncate(&mut self, length: u64) -> io::Result<()> {
        if length < self.length {
            self.store.truncate(self.collection, length)?;
            self.length = length;
        }
        Ok(())
    }

    pub fn iter(&self) -> SequenceIter<'_, 'a, T> {
        SequenceIter {
            sequence: self,
            front: 0,
            back: self.length,
        }
    }
}

#[derive(Debug)]
pub struct SequenceIter<'s, 'a, T> {
    sequence: &'s Sequence<'a, T>,
    front: u64,
    back: u64,
}

impl<T: Record> Iterator for SequenceIter<'_, '_, T> {
    type Item = io::Result<T>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.front == self.back {
            return None;
        }
        let position = self.front;
        self.front += 1;
        Some(self.sequence.get(position))
    }

    fn nth(&mut self, count: usize) -> Option<Self::Item> {
        let count = count as u64;
        if count >= self.back - self.front {
            self.front = self.back;
            return None;
        }
        self.front += count;
        self.next()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        match usize::try_from(self.back - self.front) {
            Ok(remaining) => (remaining, Some(remaining)),
            Err(_) => (usize::MAX, None),
        }
    }
}

impl<T: Record> DoubleEndedIterator for SequenceIter<'_, '_, T> {
    fn next_back(&mut self) -> Option<Self::Item> {
        if self.front == self.back {
            return None;
        }
        self.back -= 1;
        Some(self.sequence.get(self.back))
    }

    fn nth_back(&mut self, count: usize) -> Option<Self::Item> {
        let count = count as u64;
        if count >= self.back - self.front {
            self.front = self.back;
            return None;
        }
        self.back -= count;
        self.next_back()
    }
}

impl Record for u64 {
    const SIZE: usize = 8;

    fn encode(&self, bytes: &mut [u8]) {
        bytes.copy_from_slice(&self.to_le_bytes());
    }

    fn decode(bytes: &[u8]) -> io::Result<Self> {
        let bytes: [u8; 8] = bytes
            .try_into()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid integer record"))?;
        Ok(u64::from_le_bytes(bytes))
    }
}

impl<T: Record> Record for Option<T> {
    const SIZE: usize = 1 + T::SIZE;

    fn encode(&self, bytes: &mut [u8]) {
        match self {
            Some(value) => {
                bytes[0] = 1;
                value.encode(&mut bytes[1..]);
            }
            None => bytes.fill(0),
        }
    }

    fn decode(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() != Self::SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid optional record size",
            ));
        }
        match bytes[0] {
            0 => Ok(None),
            1 => T::decode(&bytes[1..]).map(Some),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid optional record tag",
            )),
        }
    }
}

impl<A: Record, B: Record> Record for (A, B) {
    const SIZE: usize = A::SIZE + B::SIZE;

    fn encode(&self, bytes: &mut [u8]) {
        self.0.encode(&mut bytes[..A::SIZE]);
        self.1.encode(&mut bytes[A::SIZE..]);
    }

    fn decode(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() != Self::SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid paired record size",
            ));
        }
        Ok((A::decode(&bytes[..A::SIZE])?, B::decode(&bytes[A::SIZE..])?))
    }
}
