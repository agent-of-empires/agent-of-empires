use std::{cell::RefCell, io};

use pulldown_cmark::disk::ByteSource;
use rusqlite::{blob::Blob, OptionalExtension};

use super::EventStore;
use crate::acp::{
    protocol::{RetainedSourceMeta, RetainedSourceQuery, RETAINED_SOURCE_BLOCK_BYTES},
    transcript::event::has_retained_payload,
};

pub(crate) enum RetainedSourceRead {
    Changed,
    InvalidCursor,
    Block {
        meta: RetainedSourceMeta,
        bytes: Vec<u8>,
    },
}

impl EventStore {
    pub(crate) fn retained_source(
        &self,
        session: &str,
        query: &RetainedSourceQuery,
    ) -> anyhow::Result<RetainedSourceRead> {
        if (query.seq.is_some() && query.history_identity.is_none())
            || (query.offset != 0 && query.seq.is_none())
        {
            return Ok(RetainedSourceRead::InvalidCursor);
        }
        let mut connection = self.conn();
        let transaction = connection.transaction()?;
        let identity = super::identity::read(&transaction, session)?;
        if query
            .history_identity
            .as_ref()
            .is_some_and(|expected| *expected != identity)
        {
            return Ok(RetainedSourceRead::Changed);
        }
        let table = self.schema.events_table();
        let (highest, lowest): (Option<i64>, Option<i64>) = transaction.query_row(
            &format!(
                "SELECT
                   (SELECT seq FROM {table} WHERE session_id = ?1 ORDER BY seq DESC LIMIT 1),
                   (SELECT seq FROM {table} WHERE session_id = ?1 ORDER BY seq ASC LIMIT 1)"
            ),
            [session],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let highest_seq = highest.map(u64::try_from).transpose()?.unwrap_or(0);
        let lowest_seq = lowest.map(u64::try_from).transpose()?;
        let (comparison, sequence) = match query.seq {
            Some(seq) => ("=", seq),
            None => (">", query.after),
        };
        let found: Option<(i64, i64)> = transaction.query_row(
            &format!("SELECT rowid, seq FROM {table} WHERE session_id = ?1 AND seq {comparison} ?2 ORDER BY seq LIMIT 1"),
            rusqlite::params![session, i64::try_from(sequence).unwrap_or(i64::MAX)], |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional()?;
        let mut meta = RetainedSourceMeta {
            history_identity: identity,
            highest_seq,
            lowest_seq,
            seq: None,
            total_bytes: 0,
            offset: query.offset,
            omitted: false,
            invalid: false,
        };
        let bytes = match found {
            Some((rowid, seq)) => {
                meta.seq = Some(u64::try_from(seq)?);
                let blob =
                    transaction.blob_open(rusqlite::MAIN_DB, table, "event_json", rowid, true)?;
                let source = BlobSource {
                    blob: &blob,
                    page: RefCell::new(Page {
                        start: 0,
                        length: 0,
                        bytes: [0; 256],
                    }),
                };
                let visible = match has_retained_payload(&source) {
                    Ok(visible) => visible,
                    Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                        meta.invalid = true;
                        false
                    }
                    Err(error) => return Err(error.into()),
                };
                if visible {
                    meta.total_bytes = blob.len() as u64;
                    if query.offset > meta.total_bytes {
                        return Ok(RetainedSourceRead::InvalidCursor);
                    }
                    let length = usize::try_from(
                        (meta.total_bytes - query.offset).min(RETAINED_SOURCE_BLOCK_BYTES as u64),
                    )?;
                    let mut bytes = vec![0; length];
                    blob.read_at_exact(&mut bytes, usize::try_from(query.offset)?)?;
                    bytes
                } else {
                    if query.offset != 0 {
                        return Ok(RetainedSourceRead::InvalidCursor);
                    }
                    meta.omitted = true;
                    Vec::new()
                }
            }
            None => {
                if query.seq.is_some() {
                    return Ok(RetainedSourceRead::InvalidCursor);
                }
                Vec::new()
            }
        };
        transaction.commit()?;
        Ok(RetainedSourceRead::Block { meta, bytes })
    }
}

struct Page {
    start: u64,
    length: usize,
    bytes: [u8; 256],
}
struct BlobSource<'a, 'connection> {
    blob: &'a Blob<'connection>,
    page: RefCell<Page>,
}
impl ByteSource for BlobSource<'_, '_> {
    fn len(&self) -> u64 {
        self.blob.len() as u64
    }
    fn read_at(&self, offset: u64, destination: &mut [u8]) -> io::Result<usize> {
        if offset > self.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid retained source offset",
            ));
        }
        let count = usize::try_from((self.len() - offset).min(destination.len() as u64))
            .map_err(io::Error::other)?;
        let mut copied = 0;
        let mut page = self.page.borrow_mut();
        while copied < count {
            let position = offset + copied as u64;
            let start = position / page.bytes.len() as u64 * page.bytes.len() as u64;
            if page.length == 0 || page.start != start {
                page.start = start;
                page.length = usize::try_from((self.len() - start).min(page.bytes.len() as u64))
                    .map_err(io::Error::other)?;
                let length = page.length;
                self.blob
                    .read_at_exact(&mut page.bytes[..length], start as usize)
                    .map_err(io::Error::other)?;
            }
            let index = (position - start) as usize;
            let length = (count - copied).min(page.length - index);
            destination[copied..copied + length]
                .copy_from_slice(&page.bytes[index..index + length]);
            copied += length;
        }
        Ok(count)
    }
}

#[cfg(test)]
mod tests;
