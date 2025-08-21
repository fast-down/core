use crate::ProgressEntry;

pub type WorkerId = usize;

#[derive(Debug)]
pub enum Event<PullError, PushError, PullStreamError, PushStreamError> {
    Pulling(WorkerId),
    PullError(WorkerId, PullError),
    PullStreamError(WorkerId, PullStreamError),
    PullProgress(WorkerId, ProgressEntry),
    PushError(WorkerId, PushError),
    PushStreamError(WorkerId, PushStreamError),
    PushProgress(WorkerId, ProgressEntry),
    FlushError(PushError),
    Finished(WorkerId),
}
