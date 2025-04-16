use crate::Progress;
use color_eyre::eyre::Error;

pub enum Event {
    Connecting(usize),
    ConnectError(usize, Error),
    Downloading(usize),
    DownloadError(usize, Error),
    DownloadProgress(Progress),
    WriteError(Error),
    WriteProgress(Progress),
    Finished(usize),
}
