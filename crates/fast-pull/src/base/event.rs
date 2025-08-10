use crate::ProgressEntry;

pub type WorkerId = usize;

#[derive(Debug)]
pub enum Event<PullError, PushError> {
    Reading(WorkerId),
    ReadError(WorkerId, PullError),
    ReadProgress(WorkerId, ProgressEntry),
    WriteError(WorkerId, PushError),
    WriteProgress(WorkerId, ProgressEntry),
    SealError(PushError),
    Finished(WorkerId),
}
