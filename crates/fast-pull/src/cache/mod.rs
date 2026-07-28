//! Pusher decorators that buffer out-of-order chunks before writing.
//!
//! Each decorator in this module wraps an inner [`Pusher`](crate::Pusher) and
//! absorbs out-of-order writes using a `BTreeMap` keyed by `range.start`,
//! flushing runs to the inner sink once a watermark is reached. Variants differ
//! in how runs are emitted: [`CacheDirectPusher`] flushes each chunk as-is,
//! [`CacheMergePusher`] coalesces each run into one contiguous buffer, and
//! [`CacheSeqPusher`] reorders runs into sequential order. [`BufWriterPusher`]
//! instead mimics `std::io::BufWriter` with a fixed-size contiguous buffer.

mod buf_writer;
mod direct;
mod merge;
mod seq;

pub use buf_writer::*;
pub use direct::*;
pub use merge::*;
pub use seq::*;
