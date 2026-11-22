use crate::{Event, Tx};
use fast_down::{
    FileId,
    http::HttpError,
    reqwest::{BuildClientError, SmartRedirectClient},
};
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
    /// A completed run's `.fd` could not be removed.
    #[error("failed to remove .fd: {0}")]
    Remove(std::io::Error),
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
    /// The range probe never answered, so whether the server can resume is
    /// unknown.
    ///
    /// This is not [`StateError::NotResumable`]: the server may well serve
    /// ranges and the probe merely failed. Starting over would replace state
    /// that is probably still resumable, so what happens is the caller's policy
    /// ([`StartPolicy::probe_unknown`](crate::StartPolicy)).
    #[error(
        "the server's range support could not be probed, so the recorded state may still be resumable"
    )]
    RangeProbeUnknown,
}

impl StateError {
    /// Whether a forced resume ([`StartMode::Forced`](crate::StartMode::Forced))
    /// would be sound for this mismatch.
    ///
    /// A forced resume continues from the recorded ranges while ignoring a
    /// change in the remote's identity headers. That is safe only when the
    /// remote still has the **same size**: the byte layout is then unchanged, so
    /// the recorded ranges still describe the same bytes.
    ///
    /// It is `false` for every other mismatch — a size change, a truncated
    /// `.part`, a server without range support, or an unreadable `.fd` — where
    /// continuing could splice two different files together or refetch the wrong
    /// span.
    #[must_use]
    pub const fn force_safe(&self) -> bool {
        matches!(
            self,
            Self::FileChanged {
                local_file_size,
                remote_file_size,
                ..
            } if *local_file_size == *remote_file_size
        )
    }
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
    Prefetch(HttpError<SmartRedirectClient>),
    /// The output path could not be computed.
    #[error(transparent)]
    GenPath(std::io::Error),
    /// A new download has nowhere to write: `save_dir` is empty and there is no
    /// `.fd` whose directory could be reused. Resuming a pinned state file does
    /// not need one.
    #[error(
        "save_dir is empty: a new download needs an output directory, while a \
         resumed .fd resolves its destination next to itself"
    )]
    MissingSaveDir,
    /// The `.fd` names the same file as its `.part` or as the output.
    ///
    /// The `.part` is derived from the state path with `with_extension`, so a
    /// state path already ending in `.part` is its own partial file; and a
    /// `filename` equal to the `.fd`'s name makes the output the state file. The
    /// run would then overwrite its state with the download (or the finished
    /// file with its own `.fd`), so the plan is refused up front.
    #[error("the .fd path collides with its .part or the output: {0}")]
    BadStatePath(PathBuf),
}

impl PlanError {
    /// Emit the [`Event`] that corresponds to this planning failure, so an
    /// observer sees the same `*Error` event it would during a resolve.
    ///
    /// The match is exhaustive on purpose: `PlanError` is `#[non_exhaustive]`
    /// only for downstream crates, so adding a variant fails to compile until it
    /// is mapped to an event here.
    pub fn emit(self, tx: &Tx) {
        let _ = match self {
            Self::BuildClient(e) => tx.send(Event::BuildClientError(e)),
            Self::Prefetch(e) => tx.send(Event::PrefetchError(e)),
            Self::GenPath(e) => tx.send(Event::GenPathError(e)),
            Self::MissingSaveDir => tx.send(Event::MissingSaveDir),
            // No dedicated event: a colliding `.fd`/`.part`/output is another
            // way of failing to resolve those paths, which is what
            // `GenPathError` reports.
            Self::BadStatePath(path) => tx.send(Event::GenPathError(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "the .fd path collides with its .part or the output: {}",
                    path.display()
                ),
            ))),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file_changed(local_size: u64, remote_size: u64) -> StateError {
        StateError::FileChanged {
            local_file_id: fast_down::FileId::default(),
            local_file_size: local_size,
            remote_file_id: fast_down::FileId::default(),
            remote_file_size: remote_size,
        }
    }

    #[test]
    fn force_safe_only_when_identity_changed_but_size_matched() {
        assert!(
            file_changed(100, 100).force_safe(),
            "same size means the byte layout is unchanged, so a forced resume is sound"
        );
    }

    #[test]
    fn force_is_unsafe_when_the_size_changed() {
        assert!(
            !file_changed(100, 200).force_safe(),
            "a size change would splice two different files together"
        );
    }

    #[test]
    fn force_is_unsafe_for_every_other_mismatch() {
        assert!(
            !StateError::Truncated {
                actual_size: 1,
                recorded_size: 2
            }
            .force_safe()
        );
        assert!(!StateError::NotResumable.force_safe());
        assert!(!StateError::NotFound(std::path::PathBuf::from("x.fd")).force_safe());
    }
}
