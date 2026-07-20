use crate::PartialConfig;
use fast_down::{ProgressEntry, UrlInfo, WorkerId};
use std::{path::PathBuf, sync::Arc};

pub enum Event {
    PrefetchError(anyhow::Error),
    GenPathError(std::io::Error),
    BuildClientError(reqwest::Error),
    BuildPusherError(std::io::Error),
    JoinError(Arc<tokio::task::JoinError>),
    RenameFailed(std::io::Error),
    Start {
        tmp_path: PathBuf,
        config_path: PathBuf,
        url_info: UrlInfo,
        parsed_config: PartialConfig,
    },

    Pulling(WorkerId),
    PullError(WorkerId, anyhow::Error),
    PullTimeout(WorkerId),
    PullProgress(WorkerId, ProgressEntry),
    Pushing(WorkerId, ProgressEntry),
    PushError(WorkerId, ProgressEntry, anyhow::Error),
    PushProgress(WorkerId, ProgressEntry),
    Flushing,
    FlushError(anyhow::Error),
    Finished(WorkerId),
}
