mod error;
mod ready;
mod record;

pub use error::*;
pub use ready::*;
pub use record::*;

use crate::utils::gen_path;
use crate::utils::path_key;
use crate::{PartialConfig, Tx, engine::prefetch};
use fast_down::fast_puller::CookieStore;
use inherit_config::ConfigLayer;
use std::path::{Path, PathBuf};
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

    /// Resolve this download against the remote: prefetch, probe for resumable
    /// state, and compute the output path.
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
    /// tried. When a `.fd` is found, its recorded `filename` and
    /// `parse_filename` decide the output name; the caller's values only fill
    /// fields the record left unset. The directory always comes from the
    /// `.fd`'s own location, so the pair can never be split. Resume is skipped
    /// entirely when the server does not support ranged requests or the caller
    /// disabled it.
    ///
    /// # Errors
    /// Returns [`PlanError`] when the HTTP client cannot be built, the prefetch
    /// exhausts its retries, or the output path cannot be computed. An empty
    /// `save_dir` on a new download and a `.fd` that names its own `.part` are
    /// local mistakes and are reported before any request is sent. A `.fd` that
    /// exists but cannot be read is reported rather than silently replaced. A
    /// `.fd` path that collides with its `.part` or with the output — for
    /// example a pinned `out.part`, or a `filename` equal to the `.fd`'s name —
    /// is refused as [`PlanError::BadStatePath`]. An unanswered range probe in
    /// front of an existing `.fd` becomes a [`ResumeOutcome::Mismatch`] carrying
    /// [`StateError::RangeProbeUnknown`], which the start policy decides on:
    /// nothing is replaced unless the caller asks for it.
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

        // The output has to be reachable from the pair: the `.part` sits next to
        // the `.fd` and `finalize` renames it there, so a destination elsewhere
        // would move the file behind the caller's back or make the rename fail
        // across volumes. A pinned `.fd` therefore owns the directory, and
        // `save_dir` applies only to a fresh download; a name template still
        // applies, resolved under that directory.
        //
        // This and the other local checks run before the probe: a configuration
        // mistake must not cost a network round-trip.
        match fd_path.as_ref().and_then(|fd| fd.parent()) {
            Some(dir) => config.save_dir = dir.to_path_buf(),
            None if config.save_dir.as_os_str().is_empty() => {
                return Err(PlanError::MissingSaveDir);
            }
            None => {}
        }

        // The `.fd`, its `.part` and the output must be three different files:
        // `part_path` only swaps the extension, so an `.fd` already ending in
        // `.part` names its own partial file (the sink would overwrite the
        // state). That is visible without the remote, so it is refused here; the
        // output collision needs the resolved name and is checked below.
        if let Some(fd) = fd_path.as_ref() {
            let tmp = crate::utils::part_path(fd);
            // The pair is fixed here, so the easy half of the collision check — a
            // state file naming its own partial file — needs no response.
            if path_key(fd) == path_key(&tmp) {
                return Err(PlanError::BadStatePath(fd.clone()));
            }
            // An explicit, non-template `filename` fixes the output as well, so
            // all three pairs can be checked before the probe. A template leaves
            // it to `plan_output`, once the response has named the file.
            if let Some(Ok(resolved)) = crate::utils::gen_path_from_name(url, &config)
                && !crate::utils::paths_are_distinct(fd, &tmp, &resolved)
            {
                return Err(PlanError::BadStatePath(fd.clone()));
            }
        }

        let cookie_store = CookieStore::new(config.cookie_store);
        let (info, resp) = prefetch(url, &config, &cookie_store, tx).await?;

        // Whether the caller pinned a specific `.fd` (via [`State::at_path`]). A
        // `Fresh` outcome keeps that pin, and a `.fd` whose `.part` is missing
        // keeps its own location even unpinned; otherwise `start` claims a free
        // pair.
        let caller_pinned = fd_path.is_some();
        // The `.fd` to probe: the pinned one, or the default next to the output
        // the caller's configuration resolves to. A record found there may still
        // move the output afterwards, but the state that was read stays this
        // file.
        let probe_fd_path = match fd_path {
            Some(fd) => fd,
            None => gen_path(url, &info, &config)
                .map_err(PlanError::GenPath)?
                .with_added_extension("fd"),
        };

        let (state, plan) = plan_output(url, &info, &config, &partial, &probe_fd_path).await?;
        let final_path = plan.final_path;

        // A clean, caller-configured record carrying the current identity but no
        // progress: what every `Fresh` path falls back to. Built lazily, so a
        // resume that never falls back pays for no copy of the configuration.
        let fresh_record = || {
            let mut fresh_partial = partial.clone();
            if let Some((filename, parse_filename)) = &plan.recorded_name {
                if fresh_partial.filename.is_none() {
                    fresh_partial.filename.clone_from(filename);
                }
                if fresh_partial.parse_filename.is_none() {
                    fresh_partial.parse_filename = *parse_filename;
                }
            }
            let mut fresh = Record::new(url.clone(), fresh_partial);
            fresh.refresh_identity(&info);
            fresh
        };
        // Where the `.fd`/`.part` pair lives is decided here, once: a `.fd` that
        // was read keeps its location whatever its content turns out to mean,
        // while a run that read nothing may still claim a free pair.
        let (record, outcome, fd_path) = match state {
            ResumeState::Loaded(old) => {
                let (record, outcome) =
                    classify(*old, url, &info, &partial, &probe_fd_path, &fresh_record).await;
                (record, outcome, Some(probe_fd_path))
            }
            // Nothing was read and the caller named no path: the location is
            // undecided, so `start` claims a free pair.
            ResumeState::Disabled | ResumeState::Missing => (
                fresh_record(),
                ResumeOutcome::Fresh,
                caller_pinned.then_some(probe_fd_path),
            ),
            // The `.fd` cannot be used, but it is still there and still names the
            // pair: keep it so the next start finds it again.
            ResumeState::Broken(e) => (
                fresh_record(),
                ResumeOutcome::Mismatch(e),
                Some(probe_fd_path),
            ),
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

/// What the probed `.fd` means for this run.
enum ResumeState {
    /// The caller disabled resume, so no state was read.
    Disabled,
    /// A `.fd` was read successfully. Boxed because a `Record` carries the whole
    /// persisted configuration, which dwarfs the other variants.
    Loaded(Box<Record>),
    /// There is no `.fd` at the probed path.
    Missing,
    /// A `.fd` exists but cannot be read or decoded.
    Broken(StateError),
}

/// What output-path resolution hands on to the rest of `build`.
struct PlannedOutput {
    /// The file this run will rename the `.part` to.
    final_path: PathBuf,
    /// The record's name fields, when a record was read. A `Fresh` fallback must
    /// keep them so it keeps resolving the same output.
    recorded_name: Option<(Option<String>, Option<bool>)>,
}

/// Read any recorded state and resolve the output path against it.
///
/// The record remembers where its download was heading, so its configuration
/// wins and the caller's only fills what the record left unset. A `Fresh`
/// fallback keeps the record's name fields for the same reason: a run
/// interrupted before completion must resolve the same output when it resumes.
///
/// Reading the record here rather than in `build` is what keeps the output name
/// stable: the path is computed from the state that is about to be continued.
#[allow(clippy::result_large_err)]
async fn plan_output(
    url: &Url,
    info: &fast_down::UrlInfo,
    config: &crate::Config,
    partial: &PartialConfig,
    probe_fd_path: &std::path::Path,
) -> Result<(ResumeState, PlannedOutput), PlanError> {
    let state = if config.resume {
        match Record::load(probe_fd_path).await {
            Ok(record) => ResumeState::Loaded(Box::new(record)),
            Err(StateError::NotFound(_)) => ResumeState::Missing,
            Err(e) => ResumeState::Broken(e),
        }
    } else {
        ResumeState::Disabled
    };
    let recorded = match &state {
        ResumeState::Loaded(record) => Some(record.as_ref()),
        ResumeState::Disabled | ResumeState::Missing | ResumeState::Broken(_) => None,
    };

    let path_partial = recorded.map_or_else(
        || partial.clone(),
        |old| {
            let mut recorded = old.config.clone();
            recorded.inherit_from(partial);
            recorded
        },
    );
    let mut path_config = path_partial.build();
    // The directory always comes from this run: a pinned `.fd` owns it, and a
    // new download uses the caller's `save_dir`.
    path_config.save_dir.clone_from(&config.save_dir);
    let final_path = gen_path(url, info, &path_config).map_err(PlanError::GenPath)?;

    // Only the name is shared with the caller's layer; every other field stays
    // the caller's, because a fresh start must not inherit the record's tuning.
    let recorded_name =
        recorded.map(|old| (old.config.filename.clone(), old.config.parse_filename));

    // With the name resolved, the last collision is visible: an output named
    // like the `.fd` makes `finalize`'s rename replace the state with the
    // finished file.
    if !crate::utils::paths_are_distinct(
        probe_fd_path,
        &crate::utils::part_path(probe_fd_path),
        &final_path,
    ) {
        return Err(PlanError::BadStatePath(probe_fd_path.to_path_buf()));
    }
    Ok((
        state,
        PlannedOutput {
            final_path,
            recorded_name,
        },
    ))
}

/// Decide what a read `.fd` means for this run.
///
/// Every judgement reads the OLD record and runs before any mutation: `validate`
/// compares the recorded identity, and `part_state` measures the file against
/// the recorded progress, while `merge_config` overwrites the progress.
///
/// The pair's location is not this function's to decide: a record that was read
/// stays where it was found, whatever the verdict.
async fn classify(
    mut old: Record,
    url: &Url,
    info: &fast_down::UrlInfo,
    partial: &PartialConfig,
    probe_fd_path: &Path,
    fresh_record: impl Fn() -> Record,
) -> (Record, ResumeOutcome) {
    let tmp = crate::utils::part_path(probe_fd_path);
    let validation = old.validate(info);
    let part = old.part_state(&tmp).await;

    // The caller's URL always wins; the identity is only refreshed when the
    // state still describes this file, so a `Mismatch` keeps its self-consistent
    // old identity.
    old.set_url(url);
    if validation.is_ok() {
        old.refresh_identity(info);
    }
    old.merge_config(partial);

    // The probe never answered, so whether this server can resume is unknown: it
    // may well serve ranges. Report that instead of reading it as "no ranges",
    // and keep the recorded progress — a policy that forces the resume needs it.
    match info.range_probe {
        fast_down::RangeProbe::Supported | fast_down::RangeProbe::Unsupported => {}
        // `Unknown`, and any state a newer `fast-down` adds: range support is not
        // established, so the recorded state may still be resumable.
        _ => {
            return (old, ResumeOutcome::Mismatch(StateError::RangeProbeUnknown));
        }
    }

    // A `.fd` exists, but the server cannot serve ranges: resume is impossible
    // whatever the `.part` looks like.
    if !info.fast_download {
        return (
            fresh_record(),
            ResumeOutcome::Mismatch(StateError::NotResumable),
        );
    }

    match (validation, part) {
        // A `.part` shorter than the recorded progress is unsafe to continue
        // whatever the identity verdict says: the sink would extend it with
        // zeros over the missing span and the run would still report completion.
        // Report the truncation (over a possible identity change) so a `Forced`
        // policy, which accepts a same-size identity change, cannot force a
        // resume over the missing bytes.
        (_, PartState::Short { actual, recorded }) => (
            old,
            ResumeOutcome::Mismatch(StateError::Truncated {
                actual_size: actual,
                recorded_size: recorded,
            }),
        ),
        (Err(e), _) => (old, ResumeOutcome::Mismatch(e)),
        (Ok(()), PartState::Ready) => (old, ResumeOutcome::Resumable),
        // The `.part` is gone: nothing safe to continue, but the `.fd` still
        // names the pair and remembers its progress. Keep that location so a
        // later start reuses it instead of orphaning it under a new `name (1)`
        // stem.
        (Ok(()), PartState::Missing) => (fresh_record(), ResumeOutcome::Fresh),
    }
}
