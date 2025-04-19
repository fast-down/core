use crate::Progress;
use color_eyre::eyre::Error;

#[derive(Debug)]
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
