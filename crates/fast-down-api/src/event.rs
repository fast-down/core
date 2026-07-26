use crate::PartialConfig;
use fast_down::{ProgressEntry, UrlInfo, WorkerId, reqwest::ReqwestResponseError};
use std::{path::PathBuf, sync::Arc};
use thiserror::Error;

/// Errors that can occur when an explicit [`DownloadHandle::resume`](crate::DownloadHandle::resume)
/// cannot continue an interrupted download.
///
/// These are surfaced through [`Event::ResumeError`] so the caller is notified
/// instead of silently falling back to a full re-download.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ResumeError {
    /// No `.fd` state file exists for this download, so there is nothing to resume from.
    #[error("no .fd state file to resume from")]
    NoStateFile,
    /// The remote file changed (size / etag / last-modified mismatch), so resuming would corrupt the output.
    #[error("remote file changed, cannot resume")]
    FileChanged,
    /// The server does not support resumable (range) downloads.
    #[error("server does not support resumable download")]
    NotResumable,
}

#[allow(clippy::large_enum_variant)]
pub enum Event {
    Prefetch(UrlInfo),
    PrefetchError(ReqwestResponseError),
    GenPathError(std::io::Error),
    BuildClientError(reqwest::Error),
    BuildPusherError(std::io::Error),
    JoinError(Arc<tokio::task::JoinError>),
    RenameFailed(std::io::Error),
    /// Emitted after the `.part` file is successfully renamed to its final
    /// destination. Carries the actual landing path, which in unique mode may
    /// differ from the originally-planned name (e.g. `xxx (1).mp4`) when the
    /// target got occupied during the download.
    Renamed(PathBuf),
    Start {
        tmp_path: PathBuf,
        config_path: PathBuf,
        parsed_config: PartialConfig,
    },
    /// Emitted when a download resumes from a previously-saved state, before
    /// [`Event::Start`]. Carries the progress that will be continued from and
    /// the total file size, so a UI can show e.g. "resuming from 42%".
    Resumed {
        config_path: PathBuf,
        progress: Vec<ProgressEntry>,
        size: u64,
    },
    /// Emitted when an explicit `resume()` call cannot continue the download.
    /// Unlike `download()` (which silently falls back to a full re-download),
    /// `resume()` reports the failure so the caller can decide what to do.
    ResumeError(ResumeError),

    Pulling(WorkerId),
    PullError(WorkerId, anyhow::Error),
    PullTimeout(WorkerId),
    PullProgress(WorkerId, ProgressEntry),
    Pushing(WorkerId, ProgressEntry),
    PushError(WorkerId, ProgressEntry, anyhow::Error),
    PushProgress(ProgressEntry),
    Flushing,
    FlushError(anyhow::Error),
    Finished(WorkerId),
}
