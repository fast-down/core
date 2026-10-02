//! Glue between [`fast_down`] and the state machine in [`crate::state`].
//!
//! Everything that talks to the engine lives here, so `state` stays pure data
//! and decision logic that can be tested without a server.

mod pipeline;
mod progress;
mod runner;

#[expect(
    clippy::redundant_pub_crate,
    reason = "`engine` is a private module; `pub(crate)` keeps these out of the `pub use engine::*` glob"
)]
pub(crate) use pipeline::build_pipeline;
#[expect(
    clippy::redundant_pub_crate,
    reason = "`engine` is a private module; `pub(crate)` keeps these out of the `pub use engine::*` glob"
)]
pub(crate) use runner::run;
pub use runner::{IdentityRecovery, Recovery, StartMode, StartPolicy};

mod prefetch;
pub use prefetch::*;

use tokio::fs::OpenOptions;

/// Open an existing file for read/write without truncating or creating it.
fn open_existing() -> OpenOptions {
    let mut opts = OpenOptions::new();
    opts.read(true).write(true).truncate(false).create(false);
    opts
}

/// Open a file for read/write, creating it if absent but never truncating.
fn open_create() -> OpenOptions {
    let mut opts = OpenOptions::new();
    opts.read(true).write(true).truncate(false).create(true);
    opts
}

/// Atomically create a file, failing if it already exists.
fn open_create_new() -> OpenOptions {
    let mut opts = OpenOptions::new();
    opts.read(true).write(true).truncate(false).create_new(true);
    opts
}
