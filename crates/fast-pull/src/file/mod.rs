//! File-backed [`Pusher`](crate::Pusher) implementations (feature `file`).
//!
//! Provides [`StdFilePusher`] (raw `std::fs::File` random-access writes),
//! [`MmapFilePusher`] (memory-mapped zero-copy writes), and [`CacheFilePusher`]
//! (a ready-made `CacheSeqPusher<BufWriterPusher<StdFilePusher>>` stack).
//! Enabled by the `file` feature.

mod cache_std;
mod mmap;
mod std;

pub use cache_std::*;
pub use mmap::*;
pub use std::*;
