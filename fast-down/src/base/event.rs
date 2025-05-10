use crate::Progress;
use color_eyre::eyre::Error;

pub type WorkerId = usize;

#[derive(Debug)]
pub enum Event {
    Connecting(WorkerId),
    ConnectError(WorkerId, Error),
    Downloading(WorkerId),
    DownloadError(WorkerId, Error),
    DownloadProgress(Progress),
    WriteError(Error),
    WriteProgress(Progress),
    Finished(WorkerId),
    Abort(WorkerId),
}
