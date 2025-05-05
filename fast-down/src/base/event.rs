use crate::Progress;
use color_eyre::eyre::Error;

pub type WorkerId = usize;

#[derive(Debug)]
pub enum Event {
    Connecting(WorkerId),
    ConnectError(WorkerId, Error),
    Downloading(WorkerId),
    DownloadError(WorkerId, Error),
    DownloadProgress(Vec<Progress>),
    WriteError(Error),
    WriteProgress(Vec<Progress>),
    Finished(WorkerId),
    Abort(WorkerId),
}
