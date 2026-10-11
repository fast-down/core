#![doc = include_str!("../README.md")]

pub mod state;
pub use state::{PartState, PlanError, Ready, Record, ResumeOutcome, State, StateError};

mod engine;
pub use engine::{Ctx, IdentityRecovery, Recovery, RunHandle, StartMode, StartPolicy};

mod config;
pub use config::*;

pub mod event;
pub use event::*;

mod error;
pub use error::TaskError;

pub(crate) mod utils;

pub use fast_down;
pub use tokio_util::sync::CancellationToken;

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
