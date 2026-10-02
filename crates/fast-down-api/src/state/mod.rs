mod error;
mod path;
mod ready;
mod record;

pub use error::*;
pub use path::*;
pub use ready::*;
pub use record::*;

use crate::utils::gen_path;
use crate::{PartialConfig, Tx, engine::prefetch};
use inherit_config::ConfigLayer;
use std::path::{Path, PathBuf};
use url::Url;

/// A download that has not been resolved against the remote yet.
///
/// This is the single input phase: it holds the persisted [`Record`] and,
/// optionally, where its `.fd` lives. `fd_path = None` means "no state file
/// picked yet" — [`State::build`] probes the base `<final>.fd`, and [`Ready`]
/// carries `None` through so `start` claims a free name. `fd_path = Some` pins
/// the location, either because the caller loaded an existing `.fd` or chose
/// one explicitly.
///
/// [`State::new`] and [`State::load`] are the two ways in; both only differ in
/// whether the record already carries remote identity and progress.
#[derive(Debug)]
pub struct State {
    pub record: Record,
    pub fd_path: Option<PathBuf>,
}

impl State {
    /// Start from a URL with no persisted state yet.
    #[must_use]
    pub const fn new(url: Url, config: PartialConfig) -> Self {
        Self {
            record: Record::from_url(url, config),
            fd_path: None,
        }
    }

    /// Load an existing `.fd` state file, pinning its location.
    ///
    /// # Errors
    /// Returns [`StateError::NotFound`] when there is no `.fd` at `fd_path`,
    /// [`StateError::Open`] for any other read failure, or
    /// [`StateError::Decode`] when the contents are not valid TOML.
    pub async fn load(fd_path: impl AsRef<Path>) -> Result<Self, StateError> {
        let fd_path = fd_path.as_ref();
        Ok(Self {
            record: Record::load(fd_path).await?,
            fd_path: Some(fd_path.to_path_buf()),
        })
    }

    /// Resolve this download against the remote: prefetch, compute the output
    /// path, and probe for resumable state.
    ///
    /// Nothing is written to disk. The returned [`Ready`] keeps the live
    /// prefetch response for `start` to reuse, and reports what starting would
    /// do:
    ///
    /// * [`ResumeOutcome::Resumable`] — a valid `.fd` with a usable `.part`.
    /// * [`ResumeOutcome::Mismatch`] — state exists but cannot be continued.
    /// * [`ResumeOutcome::Fresh`] — nothing to continue from; `start` claims a
    ///   free `.fd`/`.part` pair.
    ///
    /// A pinned `fd_path` is probed exactly; otherwise the base `<final>.fd` is
    /// tried. Resume is skipped entirely when the server does not support
    /// ranged requests or the caller disabled it.
    ///
    /// # Errors
    /// Returns [`PlanError`] when the HTTP client cannot be built, the prefetch
    /// exhausts its retries, or the output path cannot be computed. A `.fd` that
    /// exists but cannot be read is reported rather than silently replaced.
    #[allow(clippy::result_large_err)]
    pub async fn build(self, tx: Tx) -> Result<Ready, PlanError> {
        let Self { record, fd_path } = self;
        let url = record.url.clone();
        // `origin_config` is the caller's configuration with any *recorded
        // progress* stripped: a fresh start rebuilds from it, so leaking the
        // `.fd`'s `downloaded_chunk` here would make a "fresh" download skip the
        // bytes it believes are already on disk (leaving zeros) instead of
        // refetching everything. Progress is re-adopted only on the resume path,
        // via `Record::merge_config`.
        let mut partial = record.config.clone();
        partial.downloaded_chunk = None;
        let config = partial.clone().build();
        let (info, resp) = prefetch(&url, &config, &tx).await?;
        let final_path = gen_path(&url, &info, &config).map_err(PlanError::GenPath)?;

        // The caller's explicitly pinned `.fd` (from [`State::load`]), if any. It
        // is preserved through the `Fresh` outcomes so a later start can reuse
        // the pinned pair instead of claiming a new one.
        let pinned = fd_path.clone();
        let probe = fd_path
            .clone()
            .unwrap_or_else(|| final_path.with_added_extension("fd"));

        // A clean, caller-configured record carrying the current identity but no
        // progress: what every `Fresh` path falls back to.
        let fresh_record = || {
            let mut fresh = Record::from_url(url.clone(), partial.clone());
            fresh.refresh_identity(&info);
            fresh
        };

        let (record, outcome, fd_path) = if config.resume {
            match Record::load(&probe).await {
                Ok(mut old) if info.fast_download => {
                    let tmp = tmp_path_for(&probe);
                    // Every judgement reads the OLD state and must run before
                    // any mutation: `validate` compares the recorded identity,
                    // and `part_state` measures the file against the recorded
                    // progress. `merge_config` overwrites the progress, so it
                    // cannot run first.
                    let validation = old.validate(&info);
                    let part = old.part_state(&tmp).await;

                    // The caller's URL always wins; the identity is only
                    // refreshed when the state still describes this file, so a
                    // `Mismatch` keeps its self-consistent old identity.
                    old.set_url(&url);
                    if validation.is_ok() {
                        old.refresh_identity(&info);
                    }
                    old.merge_config(&partial);

                    match (validation, part) {
                        (Err(e), _) => (old, ResumeOutcome::Mismatch(e), Some(probe)),
                        (Ok(()), PartState::Ready) => (old, ResumeOutcome::Resumable, Some(probe)),
                        (Ok(()), PartState::Short { actual, recorded }) => (
                            old,
                            ResumeOutcome::Mismatch(StateError::Truncated {
                                actual_size: actual,
                                recorded_size: recorded,
                            }),
                            Some(probe),
                        ),
                        // The `.part` is gone: nothing safe to continue. An
                        // explicitly pinned `.fd` (from `State::load`) is kept
                        // so a later start can reuse it; otherwise start claims a
                        // free pair.
                        (Ok(()), PartState::Missing) => {
                            (fresh_record(), ResumeOutcome::Fresh, pinned.clone())
                        }
                    }
                }
                // A `.fd` exists, but the server cannot serve ranges: resume is
                // impossible whatever the `.part` looks like.
                Ok(_) => (
                    fresh_record(),
                    ResumeOutcome::Mismatch(StateError::NotResumable),
                    Some(probe),
                ),
                Err(StateError::NotFound(_)) => {
                    (fresh_record(), ResumeOutcome::Fresh, pinned.clone())
                }
                // Any other read/decode failure means the state exists but is
                // unusable: classify it as a mismatch so `Auto` falls back to a
                // fresh download while `Resume` reports it.
                Err(e) => (fresh_record(), ResumeOutcome::Mismatch(e), Some(probe)),
            }
        } else {
            (fresh_record(), ResumeOutcome::Fresh, pinned.clone())
        };

        Ok(Ready {
            origin_config: partial,
            record,
            fd_path,
            final_path,
            info,
            resp,
            outcome,
        })
    }
}
