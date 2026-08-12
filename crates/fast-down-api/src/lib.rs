#![doc = include_str!("../README.md")]

mod config;
mod core;
mod event;
pub(crate) mod utils;

pub use config::*;
pub use core::*;
pub use event::*;

pub use fast_down;

pub use tokio_util::sync::CancellationToken;

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
