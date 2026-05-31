use crate::ProgressEntry;

/// Numeric identifier assigned to each worker thread/task.
pub type WorkerId = usize;

/// Events emitted during a download session, received via [`DownloadResult::event_chain`](crate::DownloadResult::event_chain).
///
/// Each variant records a state change: pulling, pushing, progress, errors, or completion.
#[derive(Debug)]
pub enum Event<PullError, PushError> {
    Pulling(WorkerId),
    PullError(WorkerId, PullError),
    PullTimeout(WorkerId),
    PullProgress(WorkerId, ProgressEntry),
    Pushing(WorkerId, ProgressEntry),
    PushError(WorkerId, ProgressEntry, PushError),
    PushProgress(WorkerId, ProgressEntry),
    Flushing,
    FlushError(PushError),
    Finished(WorkerId),
}
