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

/// Sender half of the event channel, used to push [`Event`]s from the download task.
pub type Tx = crossfire::MTx<crossfire::mpmc::List<Event>>;
/// Receiver half of the event channel, used to receive [`Event`]s from the download task.
pub type Rx = crossfire::MAsyncRx<crossfire::mpmc::List<Event>>;

/// Create a new unbounded event channel for receiving download progress events.
///
/// Returns a sender (`Tx`) and receiver (`Rx`) pair.
#[must_use]
pub fn create_channel() -> (Tx, Rx) {
    crossfire::mpmc::unbounded_async()
}

/// Create a new cancellation token for use with download tasks.
///
/// Pass the token to any `DownloadTask` method (`start`, `start_with_pusher`,
/// or `start_in_memory`) to cancel the download at any time.
#[must_use]
pub fn create_cancellation_token() -> CancellationToken {
    CancellationToken::new()
}
