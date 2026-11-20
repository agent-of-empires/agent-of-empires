mod array;
mod cancel;
mod folding;
mod input;
mod labels;
mod queues;
mod spool;
mod store;

pub use array::ArrayRef;
pub use cancel::Cancellable;
pub use input::{copy_range, ByteSource, Cursor, FileSource, RangeSource, TextRef};
pub(crate) use labels::DiskLabels;
pub(crate) use queues::DiskQueues;
pub use spool::{Spool, Writer};
pub use store::{Record, Sequence, SequenceIter, Store};

pub type Event = crate::Event<
    TextRef,
    ArrayRef<TextRef>,
    ArrayRef<(TextRef, Option<TextRef>)>,
    ArrayRef<crate::Alignment>,
>;
