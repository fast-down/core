use crate::ProgressEntry;

pub type WorkerId = usize;

#[derive(Debug)]
pub enum Event<FetchError, PullError, PushError> {
    Fetching(WorkerId),
    FetchError(WorkerId, FetchError),
    Pulling(WorkerId),
    PullError(WorkerId, PullError),
    PullProgress(WorkerId, ProgressEntry),
    PushError(WorkerId, PushError),
    PushProgress(WorkerId, ProgressEntry),
    FlushError(PushError),
    Finished(WorkerId),
    Abort(WorkerId),
}
