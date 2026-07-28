use crate::{PartialConfig, StateError};
use fast_down::{ProgressEntry, UrlInfo, WorkerId, reqwest::ReqwestResponseError};
use std::{path::PathBuf, sync::Arc};

#[allow(clippy::large_enum_variant)]
pub enum Event {
    Prefetch(UrlInfo),
    PrefetchError(ReqwestResponseError),
    GenPathError(std::io::Error),
    StateSaveError(StateError),
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
    ResumeError(StateError),

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
