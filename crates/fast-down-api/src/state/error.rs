use fast_down::{FileId, reqwest::BuildClientError, reqwest::ReqwestResponseError};
use std::path::PathBuf;

/// Errors from loading, probing, or persisting a `.fd` state file.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StateError {
    /// No `.fd` state file exists at the given path.
    #[error(".fd not found: {0}")]
    NotFound(PathBuf),
    /// The `.fd` exists but could not be read.
    #[error("failed to read .fd: {0}")]
    Open(#[from] std::io::Error),
    /// The `.fd` could not be written.
    #[error("failed to save .fd: {0}")]
    Save(std::io::Error),
    /// The `.fd` contents are not valid TOML.
    #[error("failed to decode .fd: {0}")]
    Decode(#[from] toml::de::Error),
    /// The record could not be serialized to TOML.
    #[error("failed to encode .fd: {0}")]
    Encode(#[from] toml::ser::Error),
    /// The remote no longer matches the recorded identity.
    #[error(
        "remote file changed, cannot resume\n  local:  size={local_file_size}, id={local_file_id:?}\n  remote: size={remote_file_size}, id={remote_file_id:?}"
    )]
    FileChanged {
        /// Identity recorded in the `.fd`.
        local_file_id: FileId,
        /// Size recorded in the `.fd`.
        local_file_size: u64,
        /// Identity of the freshly prefetched remote.
        remote_file_id: FileId,
        /// Size of the freshly prefetched remote.
        remote_file_size: u64,
    },
    /// The `.part` file is shorter than the recorded progress.
    #[error("the .part was truncated: holds {actual_size} bytes, state records {recorded_size}")]
    Truncated {
        /// Actual `.part` size on disk.
        actual_size: u64,
        /// Size implied by the recorded progress.
        recorded_size: u64,
    },
    /// The server does not support ranged requests, so resume is impossible.
    #[error("server does not support resumable (range) requests")]
    NotResumable,
}

/// Errors from resolving a download plan ([`crate::State::build`]).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PlanError {
    /// The HTTP client could not be built.
    #[error("failed to build HTTP client: {0}")]
    BuildClient(#[from] BuildClientError),
    /// The prefetch request exhausted its retries.
    #[error(transparent)]
    Prefetch(ReqwestResponseError),
    /// The output path could not be computed.
    #[error(transparent)]
    GenPath(std::io::Error),
    /// Loading or probing the recorded state failed.
    #[error(transparent)]
    State(#[from] StateError),
}
