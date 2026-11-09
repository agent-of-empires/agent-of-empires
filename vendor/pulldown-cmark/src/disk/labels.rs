use std::io;
use std::marker::PhantomData;

use super::{folding, ByteSource, Cancellable, Record, Spool, Store, TextRef};

pub(crate) trait Labelled: Record {
    fn label(&self) -> TextRef;
}

impl Labelled for TextRef {
    fn label(&self) -> TextRef {
        *self
    }
}

impl<V: Record> Labelled for (TextRef, V) {
    fn label(&self) -> TextRef {
        self.0
    }
}

pub(crate) struct DiskLabels<'a, V> {
    store: &'a Store,
    spool: &'a Spool,
    collection: i64,
    marker: PhantomData<fn() -> V>,
}

impl<'a, V: Labelled> DiskLabels<'a, V> {
    pub(crate) fn new(store: &'a Store, spool: &'a Spool) -> io::Result<Self> {
        Ok(Self {
            store,
            spool,
            collection: store.collection()?,
            marker: PhantomData,
        })
    }

    pub(crate) fn find(&self, label: TextRef) -> io::Result<Option<V>> {
        let text = self.spool.view(label)?;
        let text = Cancellable::new(&text, self.store.cancellation());
        let key = folding::key(&text, 0..label.len)?;
        self.find_hashed(&text, label.len, &key)
    }

    pub(crate) fn insert_first(&self, value: V) -> io::Result<()> {
        let label = value.label();
        let text = self.spool.view(label)?;
        let text = Cancellable::new(&text, self.store.cancellation());
        let key = folding::key(&text, 0..label.len)?;
        if self.find_hashed(&text, label.len, &key)?.is_none() {
            self.store.insert(self.collection, &key, &value)?;
        }
        Ok(())
    }

    fn find_hashed<S: ByteSource + ?Sized>(
        &self,
        text: &S,
        length: u64,
        key: &[u8; 32],
    ) -> io::Result<Option<V>> {
        let mut after = 0;
        while let Some((id, value)) = self.store.lookup::<V>(self.collection, key, after)? {
            let label = value.label();
            let candidate = self.spool.view(label)?;
            let candidate = Cancellable::new(&candidate, self.store.cancellation());
            if folding::equal(text, 0..length, &candidate, 0..label.len)? {
                return Ok(Some(value));
            }
            after = id;
        }
        Ok(None)
    }
}

impl<V: Record> crate::storage::Dictionary<TextRef, V> for DiskLabels<'_, (TextRef, V)> {
    type Error = io::Error;

    fn lookup(&self, key: &TextRef) -> io::Result<Option<V>> {
        self.find(*key).map(|value| value.map(|(_, value)| value))
    }

    fn insert_first(&mut self, key: TextRef, value: V) -> io::Result<()> {
        DiskLabels::insert_first(self, (key, value))
    }
}

impl crate::storage::Set<TextRef> for DiskLabels<'_, TextRef> {
    type Error = io::Error;

    fn contains(&self, key: &TextRef) -> io::Result<bool> {
        self.find(*key).map(|value| value.is_some())
    }

    fn insert(&mut self, key: TextRef) -> io::Result<()> {
        self.insert_first(key)
    }
}
