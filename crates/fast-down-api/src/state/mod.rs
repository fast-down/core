mod error;
mod ready;
mod record;

pub use error::*;
pub use ready::*;
pub use record::*;

use crate::utils::gen_path;
use crate::{PartialConfig, Tx, engine::prefetch};
use fast_down::fast_puller::CookieStore;
use inherit_config::ConfigLayer;
use std::path::PathBuf;
use url::Url;

/// A download that has not been resolved against the remote yet.
///
/// This is the single input phase: it holds the URL and configuration and,
/// optionally, where the `.fd` lives. `fd_path = None` means "no state file
/// picked yet" — [`State::build`] probes the base `<final>.fd` and keeps that
/// path when a `.fd` is there; with no file to keep, [`Ready`] carries `None`
/// through so `start` claims a free name. `fd_path = Some` pins the location,
/// either because the caller located an existing `.fd` or chose one explicitly.
///
/// **`State` never reads the disk.** Reading is the whole job of
/// [`State::build`], which reads the pinned (or default) `.fd` exactly once and
/// classifies it against the freshly prefetched remote. To *inspect* an existing
/// `.fd` offline — list what is resumable, or validate a file you located
/// yourself — use [`Record::load`], which decodes it without any network access.
#[derive(Debug, Clone)]
pub struct State {
    /// The persisted record. Until [`State::build`] runs it only carries the
    /// URL and the caller's configuration; the identity and progress are read
    /// from the `.fd` by `build`.
    pub record: Record,
    /// The `.fd` location pinned by [`State::at_path`]; `None` until resolved.
    pub fd_path: Option<PathBuf>,
}

impl State {
    /// Start from a URL with no persisted state yet.
    #[must_use]
    pub const fn new(url: Url, config: PartialConfig) -> Self {
        Self {
            record: Record::new(url, config),
            fd_path: None,
        }
    }

    /// Start from a URL, pinning the exact `.fd` to probe during [`build`].
    ///
    /// Nothing is read here: `build` reads that one file and classifies it
    /// against the remote. Use this when the caller already knows the URL and
    /// wants to resume from a specific state file it located itself.
    ///
    /// The pinned path also decides where the finished file lands: the `.part`
    /// lives next to the `.fd` and is renamed from there, so [`build`] resolves
    /// the destination under the `.fd`'s own directory and ignores `save_dir`.
    /// A `filename` template still applies, resolved relative to that directory.
    ///
    /// [`build`]: Self::build
    #[must_use]
    pub const fn at_path(url: Url, fd_path: PathBuf, config: PartialConfig) -> Self {
        Self {
            record: Record::new(url, config),
            fd_path: Some(fd_path),
        }
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
    /// * [`ResumeOutcome::Fresh`] — nothing to continue from; `start` reuses the
    ///   known `.fd` location, or claims a free `.fd`/`.part` pair when there is
    ///   none.
    ///
    /// A pinned `fd_path` is probed exactly; otherwise the base `<final>.fd` is
    /// tried. Resume is skipped entirely when the server does not support
    /// ranged requests or the caller disabled it.
    ///
    /// # Errors
    /// Returns [`PlanError`] when the HTTP client cannot be built, the prefetch
    /// exhausts its retries, or the output path cannot be computed. A `.fd` that
    /// exists but cannot be read is reported rather than silently replaced. A
    /// `.fd` path that collides with its `.part` or with the output — for
    /// example a pinned `out.part`, or a `filename` equal to the `.fd`'s name —
    /// is refused as [`PlanError::BadStatePath`].
    ///
    /// # Events
    /// `build` publishes on `tx` as it resolves the remote: an
    /// [`Event::Prefetch`](crate::Event::Prefetch) on success, and one
    /// [`Event::PrefetchError`](crate::Event::PrefetchError) per **retryable**
    /// prefetch attempt (the attempt that exhausts the budget is returned as
    /// [`PlanError::Prefetch`] instead, since the error cannot be cloned into
    /// both places). These are sent while `build` is still running, so a caller
    /// that wants to observe the retries should drain `tx` concurrently; a caller
    /// that only inspects the returned [`PlanError`] sees just the final failure.
    /// Nothing else is published until `start`.
    #[allow(clippy::result_large_err)]
    pub async fn build(self, tx: &Tx) -> Result<Ready, PlanError> {
        let Self { record, fd_path } = self;
        let url = &record.url;
        // `origin_config` is the caller's configuration with any *recorded
        // progress* stripped: a fresh start rebuilds from it, so leaking the
        // `.fd`'s `downloaded_chunk` here would make a "fresh" download skip the
        // bytes it believes are already on disk (leaving zeros) instead of
        // refetching everything. Progress is re-adopted only on the resume path,
        // via `Record::merge_config`.
        let mut partial = record.config.clone();
        partial.downloaded_chunk = None;
        let mut config = partial.clone().build();
        let cookie_store = CookieStore::new(config.cookie_store);
        let (info, resp) = prefetch(url, &config, &cookie_store, tx).await?;

        // The output has to be reachable from the pair: the `.part` sits next to
        // the `.fd` and `finalize` renames it there, so a destination elsewhere
        // would move the file behind the caller's back or make the rename fail
        // across volumes. A pinned `.fd` therefore owns the directory, and
        // `save_dir` applies only to a fresh download; a name template still
        // applies, resolved under that directory.
        match fd_path.as_ref().and_then(|fd| fd.parent()) {
            Some(dir) => config.save_dir = dir.to_path_buf(),
            None if config.save_dir.as_os_str().is_empty() => {
                return Err(PlanError::MissingSaveDir);
            }
            None => {}
        }
        let final_path = gen_path(url, &info, &config).map_err(PlanError::GenPath)?;

        // Whether the caller pinned a specific `.fd` (via [`State::at_path`]). A
        // `Fresh` outcome keeps that pin, and a `.fd` whose `.part` is missing
        // keeps its own location even unpinned; otherwise `start` claims a free
        // pair.
        let caller_pinned = fd_path.is_some();
        // The `.fd` to probe: the pinned one, or the default `<final>.fd`. No
        // clone: the resume paths move it into the result, and a fresh path only
        // needs `caller_pinned` to decide whether to keep it.
        let probe_fd_path = fd_path.unwrap_or_else(|| final_path.with_added_extension("fd"));

        // The `.fd`, its `.part` and the output must be three different files:
        // `part_path` only swaps the extension, so an `.fd` already ending in
        // `.part` names its own partial file (the sink would overwrite the
        // state), and an output named like the `.fd` makes `finalize`'s rename
        // replace the state with the finished file.
        if !crate::utils::paths_are_distinct(
            &probe_fd_path,
            &crate::utils::part_path(&probe_fd_path),
            &final_path,
        ) {
            return Err(PlanError::BadStatePath(probe_fd_path));
        }

        // A clean, caller-configured record carrying the current identity but no
        // progress: what every `Fresh` path falls back to.
        let fresh_record = || {
            let mut fresh = Record::new(url.clone(), partial.clone());
            fresh.refresh_identity(&info);
            fresh
        };

        let (record, outcome, fd_path) = if config.resume {
            match Record::load(&probe_fd_path).await {
                Ok(mut old) if info.fast_download => {
                    let tmp = crate::utils::part_path(&probe_fd_path);
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
                    old.set_url(url);
                    if validation.is_ok() {
                        old.refresh_identity(&info);
                    }
                    old.merge_config(&partial);

                    match (validation, part) {
                        (Err(e), _) => (old, ResumeOutcome::Mismatch(e), Some(probe_fd_path)),
                        (Ok(()), PartState::Ready) => {
                            (old, ResumeOutcome::Resumable, Some(probe_fd_path))
                        }
                        (Ok(()), PartState::Short { actual, recorded }) => (
                            old,
                            ResumeOutcome::Mismatch(StateError::Truncated {
                                actual_size: actual,
                                recorded_size: recorded,
                            }),
                            Some(probe_fd_path),
                        ),
                        // The `.part` is gone: nothing safe to continue, but the
                        // `.fd` still names the pair and remembers its progress.
                        // Keep that location so a later start reuses it instead
                        // of orphaning it under a new `name (1)` stem.
                        (Ok(()), PartState::Missing) => {
                            (fresh_record(), ResumeOutcome::Fresh, Some(probe_fd_path))
                        }
                    }
                }
                // A `.fd` exists, but the server cannot serve ranges: resume is
                // impossible whatever the `.part` looks like.
                Ok(_) => (
                    fresh_record(),
                    ResumeOutcome::Mismatch(StateError::NotResumable),
                    Some(probe_fd_path),
                ),
                Err(StateError::NotFound(_)) => (
                    fresh_record(),
                    ResumeOutcome::Fresh,
                    caller_pinned.then_some(probe_fd_path),
                ),
                // Any other read/decode failure means the state exists but is
                // unusable: classify it as a mismatch so `Auto` falls back to a
                // fresh download while `Resume` reports it.
                Err(e) => (
                    fresh_record(),
                    ResumeOutcome::Mismatch(e),
                    Some(probe_fd_path),
                ),
            }
        } else {
            (
                fresh_record(),
                ResumeOutcome::Fresh,
                caller_pinned.then_some(probe_fd_path),
            )
        };

        Ok(Ready {
            origin_config: partial,
            record,
            fd_path,
            final_path,
            info,
            resp,
            outcome,
            cookie_store,
        })
    }
}
