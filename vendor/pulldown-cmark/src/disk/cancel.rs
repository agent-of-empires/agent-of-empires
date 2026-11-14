use std::io;
use std::sync::atomic::{AtomicBool, Ordering};

use super::ByteSource;

#[derive(Debug)]
pub struct Cancellable<'a, S: ByteSource + ?Sized> {
    source: &'a S,
    cancelled: &'a AtomicBool,
}

impl<'a, S: ByteSource + ?Sized> Cancellable<'a, S> {
    pub fn new(source: &'a S, cancelled: &'a AtomicBool) -> Self {
        Self { source, cancelled }
    }
}

impl<S: ByteSource + ?Sized> ByteSource for Cancellable<'_, S> {
    fn len(&self) -> u64 {
        self.source.len()
    }

    fn read_at(&self, offset: u64, destination: &mut [u8]) -> io::Result<usize> {
        if self.cancelled.load(Ordering::Relaxed) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "parser read cancelled",
            ));
        }
        self.source.read_at(offset, destination)
    }
}
