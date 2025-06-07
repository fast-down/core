use crate::ProgressEntry;

pub type WorkerId = usize;

#[derive(Debug)]
pub enum ConnectErrorKind {
    Reqwest(reqwest::Error),
    /// The server returned a non-206 status code when use Range header.
    StatusCode(reqwest::StatusCode),
}

impl std::fmt::Display for ConnectErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectErrorKind::Reqwest(err) => write!(f, "Reqwest error: {}", err),
            ConnectErrorKind::StatusCode(code) => write!(
                f,
                "Server returned non-206 status code: {} {}",
                code.as_u16(),
                code.canonical_reason().unwrap_or("unknown status")
            ),
        }
    }
}

impl std::error::Error for ConnectErrorKind {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConnectErrorKind::Reqwest(err) => Some(err),
            ConnectErrorKind::StatusCode(_) => None,
        }
    }
}

#[derive(Debug)]
pub enum Event {
    Connecting(WorkerId),
    ConnectError(WorkerId, ConnectErrorKind),
    Downloading(WorkerId),
    DownloadError(WorkerId, std::io::Error),
    DownloadProgress(ProgressEntry),
    WriteError(std::io::Error),
    WriteProgress(ProgressEntry),
    Finished(WorkerId),
    Abort(WorkerId),
}
