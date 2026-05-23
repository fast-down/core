#![doc = include_str!("../README.md")]

mod config;
mod download;
mod error;
mod event;

pub use config::*;
pub use download::*;
pub use error::*;
pub use event::*;
pub use fast_down;
pub use fast_down::{
    AnyError, BoxPusher, CacheDirectPusher, CacheMergePusher, CacheSeqPusher, DownloadResult,
    Event as RawEvent, FileId, InvertIter, Merge, ProgressEntry, Proxy, PullResult, PullStream,
    Puller, PullerError, Pusher, Total, UrlInfo, WorkerId, fast_puller, getifaddrs, handle, http,
    invert, mock, multi, reqwest as reqwest_adapter, single,
};

#[cfg(feature = "file")]
pub use fast_down::file;
#[cfg(feature = "mem")]
pub use fast_down::mem;

use tokio_util::sync::CancellationToken;

pub type Tx = crossfire::MTx<crossfire::mpmc::List<Event>>;
pub type Rx = crossfire::MAsyncRx<crossfire::mpmc::List<Event>>;

#[must_use]
pub fn create_channel() -> (Tx, Rx) {
    crossfire::mpmc::unbounded_async()
}

#[must_use]
pub fn create_cancellation_token() -> CancellationToken {
    CancellationToken::new()
}
