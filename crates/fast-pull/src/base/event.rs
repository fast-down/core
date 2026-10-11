//! Download lifecycle events emitted on the
//! [`event_chain`](crate::DownloadResult::event_chain).

use crate::ProgressEntry;

/// Numeric identifier assigned to each worker thread/task.
pub type WorkerId = usize;

/// Events emitted during a download session, received via [`DownloadResult::event_chain`](crate::DownloadResult::event_chain).
///
/// Each variant records a state change: pulling, pushing, progress, errors, or completion.
///
/// Deliberately **not** `#[non_exhaustive]`: `fast-down`'s forwarding layer
/// matches it exhaustively, so a new variant forces that layer to be updated
/// rather than letting the event be dropped silently.
#[derive(Debug)]
pub enum Event<PullError, PushError> {
    /// A worker started fetching its assigned range (carries its id).
    Pulling(WorkerId),
    /// A worker failed to fetch its range.
    PullError(WorkerId, PullError),
    /// A worker's fetch exceeded its time budget and was aborted.
    PullTimeout(WorkerId),
    /// A worker pulled a range into memory (not yet written).
    PullProgress(WorkerId, ProgressEntry),
    /// A worker is handing a pulled range to the sink.
    Pushing(WorkerId, ProgressEntry),
    /// A worker failed to write a range to the sink.
    PushError(WorkerId, ProgressEntry, PushError),
    /// A range was successfully written to the sink.
    PushProgress(ProgressEntry),
    /// The sink is being flushed.
    Flushing,
    /// Flushing the sink failed.
    FlushError(PushError),
    /// A worker completed its assigned range and exited.
    Finished(WorkerId),
}
