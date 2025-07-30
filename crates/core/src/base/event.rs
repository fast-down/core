use crate::ProgressEntry;

pub type WorkerId = usize;

#[derive(thiserror::Error, Debug)]
pub enum ConnectErrorKind {
    #[error("Reqwest error: {0}")]
    Reqwest(#[from] reqwest::Error),
    /// The server returned a non-206 status code when use Range header.
    #[error("Server returned non-206 status code: {0}")]
    StatusCode(reqwest::StatusCode),
}

#[derive(Debug)]
pub enum Event {
    Connecting(WorkerId),
    ConnectError(WorkerId, ConnectErrorKind),
    Downloading(WorkerId),
    DownloadError(WorkerId, reqwest::Error),
    DownloadProgress(WorkerId, ProgressEntry),
    WriteError(WorkerId, std::io::Error),
    WriteProgress(WorkerId, ProgressEntry),
    Finished(WorkerId),
    Abort(WorkerId),
}
