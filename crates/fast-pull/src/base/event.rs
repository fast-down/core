use crate::ProgressEntry;

pub type WorkerId = usize;

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
