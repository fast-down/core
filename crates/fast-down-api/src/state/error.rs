use fast_down::{FileId, reqwest::ReqwestResponseError};
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error(".fd not found: {0}")]
    NotFound(PathBuf),
    #[error("failed to read .fd: {0}")]
    Open(#[from] std::io::Error),
    #[error("failed to save .fd: {0}")]
    Save(std::io::Error),
    #[error("failed to decode .fd: {0}")]
    Decode(#[from] toml::de::Error),
    #[error("failed to encode .fd: {0}")]
    Encode(#[from] toml::ser::Error),
    #[error(
        "remote file changed, cannot resume\n  local:  size={local_file_size}, id={local_file_id:?}\n  remote: size={remote_file_size}, id={remote_file_id:?}"
    )]
    FileChanged {
        local_file_id: FileId,
        local_file_size: u64,
        remote_file_id: FileId,
        remote_file_size: u64,
    },
    #[error("the .part was truncated: holds {actual_size} bytes, state records {recorded_size}")]
    Truncated {
        actual_size: u64,
        recorded_size: u64,
    },
    #[error("server does not support resumable (range) requests")]
    NotResumable,
}

#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    #[error("failed to build HTTP client: {0}")]
    BuildClient(#[from] reqwest::Error),
    #[error(transparent)]
    Prefetch(ReqwestResponseError),
    #[error(transparent)]
    GenPath(std::io::Error),
    #[error(transparent)]
    State(#[from] StateError),
}
