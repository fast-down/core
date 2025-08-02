use crate::ProgressEntry;

pub type WorkerId = usize;

#[derive(Debug)]
pub enum Event<ReadError, WriteError> {
    Reading(WorkerId),
    ReadError(WorkerId, ReadError),
    ReadProgress(WorkerId, ProgressEntry),
    WriteError(WorkerId, WriteError),
    WriteProgress(WorkerId, ProgressEntry),
    FlushError(WriteError),
    Finished(WorkerId),
    Abort(WorkerId),
}
