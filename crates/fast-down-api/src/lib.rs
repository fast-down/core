#![doc = include_str!("../README.md")]

/// State machine for resolving, resuming, and persisting downloads.
pub mod state;
pub use state::{PartState, PlanError, Ready, Record, ResumeOutcome, State, StateError};

mod engine;
pub use engine::{Ctx, IdentityRecovery, Recovery, RunHandle, StartMode, StartPolicy};

mod rate;

mod config;
pub use config::*;

/// Download lifecycle events and aggregate statistics.
pub mod event;
pub use event::*;

mod queue;
pub use queue::{Queue, QueueError, QueueEvent, QueueRx, QueueTx, create_queue_channel};

mod error;
pub use error::TaskError;

pub(crate) mod utils;

pub use fast_down;
pub use tokio_util::sync::CancellationToken;

/// Sending half of the event channel.
pub type Tx = crossfire::MTx<crossfire::mpmc::List<Event>>;
/// Receiving half of the event channel.
pub type Rx = crossfire::MAsyncRx<crossfire::mpmc::List<Event>>;

/// Create an unbounded async event channel for a run's [`Event`]s.
///
/// The channel is unbounded: a consumer that stops draining while the run keeps
/// emitting per-chunk events grows memory. Keep draining, or abort the run.
#[must_use]
pub fn create_channel() -> (Tx, Rx) {
    crossfire::mpmc::unbounded_async()
}
