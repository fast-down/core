#![doc = include_str!("../README.md")]
#![allow(clippy::missing_errors_doc)]

pub mod state;
pub use state::{
    PartState, PlanError, Ready, Record, ResumeOutcome, State, StateError, tmp_path_for,
};

mod engine;
pub use engine::*;

mod config;
pub use config::*;

pub mod event;
pub use event::*;

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
