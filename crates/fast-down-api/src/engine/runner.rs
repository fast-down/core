//! Single-owner run loop: claims the `.fd`/`.part` pair, drives the engine, and
//! periodically persists the state.
//!
//! The loop owns the [`Record`] outright, so nothing has to be shared behind a
//! mutex. Persisting is handed to a background task that only ever sees a cloned
//! snapshot; it reports failures back over its own channel (the public [`Tx`] is
//! not `Send`, so the worker cannot hold one), and the bounded snapshot channel
//! makes a slow disk skip a tick instead of stalling the event loop.

use super::{build_pipeline, open_create, open_create_new, progress};
use crate::{
    Event, PartialConfig, TerminationReason, Tx,
    engine::progress::RateEstimator,
    state::{Record, ResumeOutcome, StateError, tmp_path_for},
};
use fast_down::{UrlInfo, invert, multi::download_multi, single::download_single};
use inherit_config::ConfigLayer;
use path_helper::IterStemExt;
use reqwest::Response;
use std::{
    fmt::Display,
    io,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tokio::{fs, sync::mpsc, time::MissedTickBehavior};
use tokio_util::sync::CancellationToken;

/// How often the `.fd` state is persisted while a run is in flight.
const PERSIST_GAP: Duration = Duration::from_secs(1);
/// Floor for the progress cadence, so a zero `progress_emit_gap` cannot panic
/// `tokio::time::interval`.
const MIN_PROGRESS_GAP: Duration = Duration::from_millis(1);

/// A shorthanded preset for how a resolved download should be started.
///
/// Each variant maps onto a [`StartPolicy`] (see [`From<StartMode>`]); use
/// [`StartPolicy`] directly when you need per-dimension control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartMode {
    /// Resume when possible, otherwise download the whole file.
    Auto,
    /// Resume only: refuse (via [`Event::ResumeError`]) when the recorded state
    /// exists but no longer describes the remote.
    Resume,
    /// Ignore any recorded progress and download the whole file.
    Fresh,
    /// Continue from a mismatched state when the size still matches.
    Forced,
}

impl From<StartMode> for StartPolicy {
    fn from(mode: StartMode) -> Self {
        match mode {
            StartMode::Auto => Self::auto(),
            StartMode::Resume => Self::resume(),
            StartMode::Fresh => Self::fresh(),
            StartMode::Forced => Self::forced(),
        }
    }
}

/// What to do when recorded state exists but cannot be resumed cleanly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Recovery {
    /// Ignore the recorded state and download the whole file.
    #[default]
    Fresh,
    /// Report [`Event::ResumeError`] and stop.
    Fail,
}

/// What to do when the remote's identity headers changed but its **size** still
/// matches, so the recorded byte ranges still describe the same bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IdentityRecovery {
    /// Ignore the recorded state and download the whole file.
    #[default]
    Fresh,
    /// Report [`Event::ResumeError`] and stop.
    Fail,
    /// Continue from the recorded state despite the identity change. Sound only
    /// because the size matches — the byte layout is unchanged.
    Force,
}

/// Per-dimension policy for how [`Ready::start`](crate::Ready::start) handles a
/// recorded `.fd`/`.part` that cannot be resumed cleanly.
///
/// Each field covers one independent reason a resume might not be possible; set
/// exactly the ones you care about and take the rest from [`Default`] (which is
/// [`StartPolicy::auto`]). The common combinations are available as presets via
/// [`StartMode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StartPolicy {
    /// Use recorded progress when it is valid. `false` ignores any recorded
    /// progress and downloads the whole file, reusing a pinned `.fd`/`.part`
    /// location when one is set.
    pub use_recorded: bool,
    /// No `.fd`, or a `.fd` whose `.part` is gone — nothing to continue.
    pub no_state: Recovery,
    /// The `.part` is shorter than the recorded progress.
    pub truncated: Recovery,
    /// The `.fd` exists but cannot be read or decoded.
    pub unreadable: Recovery,
    /// The remote file's size changed, so the recorded ranges describe
    /// different bytes.
    pub size_changed: Recovery,
    /// The server does not support ranged requests.
    pub not_resumable: Recovery,
    /// The remote's identity headers changed but the size still matches.
    pub identity_changed: IdentityRecovery,
}

impl Default for StartPolicy {
    fn default() -> Self {
        Self::auto()
    }
}

impl StartPolicy {
    /// Resume when possible, otherwise download the whole file.
    #[must_use]
    pub const fn auto() -> Self {
        Self {
            use_recorded: true,
            no_state: Recovery::Fresh,
            truncated: Recovery::Fresh,
            unreadable: Recovery::Fresh,
            size_changed: Recovery::Fresh,
            not_resumable: Recovery::Fresh,
            identity_changed: IdentityRecovery::Fresh,
        }
    }

    /// Resume this specific state, refusing (via [`Event::ResumeError`]) when it
    /// can no longer be continued; only "nothing safe to continue" falls back.
    #[must_use]
    pub const fn resume() -> Self {
        Self {
            use_recorded: true,
            no_state: Recovery::Fresh,
            truncated: Recovery::Fresh,
            unreadable: Recovery::Fail,
            size_changed: Recovery::Fail,
            not_resumable: Recovery::Fail,
            identity_changed: IdentityRecovery::Fail,
        }
    }

    /// Ignore any recorded progress and download the whole file.
    #[must_use]
    pub const fn fresh() -> Self {
        Self {
            use_recorded: false,
            no_state: Recovery::Fresh,
            truncated: Recovery::Fresh,
            unreadable: Recovery::Fresh,
            size_changed: Recovery::Fresh,
            not_resumable: Recovery::Fresh,
            identity_changed: IdentityRecovery::Fresh,
        }
    }

    /// Continue from a mismatched state when the size still matches.
    #[must_use]
    pub const fn forced() -> Self {
        Self {
            use_recorded: true,
            no_state: Recovery::Fresh,
            truncated: Recovery::Fail,
            unreadable: Recovery::Fail,
            size_changed: Recovery::Fail,
            not_resumable: Recovery::Fail,
            identity_changed: IdentityRecovery::Force,
        }
    }
}

/// The resolved decision for a single run.
enum Fate {
    /// Continue from the recorded state.
    Resume,
    /// Ignore the recorded state and download the whole file.
    Fresh,
    /// Report the mismatch and stop.
    Fail,
}

/// Resolve a [`StartPolicy`] against the probed [`ResumeOutcome`].
fn decide(policy: StartPolicy, outcome: &ResumeOutcome) -> Fate {
    if !policy.use_recorded {
        return Fate::Fresh;
    }
    match outcome {
        ResumeOutcome::Resumable => Fate::Resume,
        ResumeOutcome::Fresh => match policy.no_state {
            Recovery::Fresh => Fate::Fresh,
            Recovery::Fail => Fate::Fail,
        },
        ResumeOutcome::Mismatch(error) => match error {
            StateError::Truncated { .. } => match policy.truncated {
                Recovery::Fresh => Fate::Fresh,
                Recovery::Fail => Fate::Fail,
            },
            StateError::NotResumable => match policy.not_resumable {
                Recovery::Fresh => Fate::Fresh,
                Recovery::Fail => Fate::Fail,
            },
            StateError::FileChanged {
                local_file_size,
                remote_file_size,
                ..
            } => {
                if local_file_size == remote_file_size {
                    match policy.identity_changed {
                        IdentityRecovery::Fresh => Fate::Fresh,
                        IdentityRecovery::Fail => Fate::Fail,
                        IdentityRecovery::Force => Fate::Resume,
                    }
                } else {
                    match policy.size_changed {
                        Recovery::Fresh => Fate::Fresh,
                        Recovery::Fail => Fate::Fail,
                    }
                }
            }
            _ => match policy.unreadable {
                Recovery::Fresh => Fate::Fresh,
                Recovery::Fail => Fate::Fail,
            },
        },
    }
}

/// Run a resolved download to completion, emitting events and exactly one
/// [`Event::Terminated`] as the last event.
///
/// Claims a free `.fd`/`.part` pair when `fd_path` is `None`, or reuses the given
/// one; `policy` decides what becomes of any recorded progress.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    record: Record,
    origin_config: PartialConfig,
    fd_path: Option<PathBuf>,
    final_path: PathBuf,
    info: UrlInfo,
    resp: Response,
    outcome: ResumeOutcome,
    policy: impl Into<StartPolicy>,
    tx: Tx,
    token: CancellationToken,
) {
    let reason = drive(
        record,
        &origin_config,
        fd_path,
        final_path,
        info,
        resp,
        outcome,
        policy.into(),
        &tx,
        &token,
    )
    .await;
    let _ = tx.send(Event::Terminated(reason));
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn drive(
    mut record: Record,
    origin_config: &PartialConfig,
    fd_path: Option<PathBuf>,
    final_path: PathBuf,
    info: UrlInfo,
    resp: Response,
    outcome: ResumeOutcome,
    policy: StartPolicy,
    tx: &Tx,
    token: &CancellationToken,
) -> TerminationReason {
    // 1. Decide what to do with the probed state, per the caller's policy.
    let resuming = match decide(policy, &outcome) {
        Fate::Resume => true,
        Fate::Fresh => false,
        Fate::Fail => {
            if let ResumeOutcome::Mismatch(e) = outcome {
                let _ = tx.send(Event::ResumeError(e));
            }
            return TerminationReason::Failed;
        }
    };

    // 2. Make sure the output directory exists before claiming a scratch pair
    //    (the `.fd`/`.part` sit next to `final_path`, so `create_new` there would
    //    otherwise fail when `final_path`'s parent — e.g. a template subdir — has
    //    not been created yet).
    if !ensure_parent(&final_path, tx).await {
        return TerminationReason::Failed;
    }

    // 3. Resolve the `.fd`/`.part` pair and the progress to run with.
    let (fd_path, tmp_path) = if resuming {
        // A resumable/mismatched state always carries its location.
        let Some(fd_path) = fd_path else {
            let _ = tx.send(Event::ResumeError(StateError::NotResumable));
            return TerminationReason::Failed;
        };
        (fd_path.clone(), tmp_path_for(&fd_path))
    } else if let Some(fd_path) = fd_path {
        // Fresh over an existing pair: reuse the location and the `.part`, but
        // start from byte zero. This is how a pinned `.fd` (from `State::load`)
        // is reused even when its `.part` is missing.
        let tmp = tmp_path_for(&fd_path);
        if let Err(e) = open_create().open(&tmp).await {
            let _ = tx.send(Event::BuildPusherError(e));
            return TerminationReason::Failed;
        }
        record = fresh_over(&record, origin_config, &info);
        (fd_path, tmp)
    } else {
        match claim_pair(&final_path, tx).await {
            Ok(pair) => pair,
            Err(reason) => return reason,
        }
    };

    if let Err(e) = record.store(&fd_path).await {
        let _ = tx.send(Event::StateSaveError(e));
        return TerminationReason::Failed;
    }

    if resuming {
        let _ = tx.send(Event::Resumed {
            config_path: fd_path.clone(),
            progress: record.progress(),
            size: info.size,
        });
    }

    let config = record.config.clone().build();
    let Some((puller, pusher)) = build_pipeline(&config, &info, resp, &tmp_path, tx, token).await
    else {
        return TerminationReason::Failed;
    };

    let _ = tx.send(Event::Start {
        tmp_path: tmp_path.clone(),
        config_path: fd_path.clone(),
        parsed_config: record.config.clone(),
    });

    // 3. Construct the engine session and drive it.
    let res = if info.fast_download {
        download_multi(
            puller,
            pusher,
            fast_down::multi::DownloadOptions {
                download_chunks: invert(
                    config.downloaded_chunk.iter().cloned(),
                    info.size,
                    config.chunk_window,
                ),
                concurrent: config.threads,
                retry_gap: config.retry_gap,
                pull_timeout: config.pull_timeout,
                push_queue_cap: config.write_queue_cap,
                min_chunk_size: config.min_chunk_size,
                max_speculative: config.max_speculative,
            },
        )
    } else {
        download_single(
            puller,
            pusher,
            fast_down::single::DownloadOptions {
                retry_gap: config.retry_gap,
                push_queue_cap: config.write_queue_cap,
            },
        )
    };

    let loaded_elapsed = record.elapsed;
    let start = Instant::now();
    let mut rate = RateEstimator::default();

    let (snap_tx, snap_rx) = mpsc::channel::<Record>(1);
    let (err_tx, mut err_rx) = mpsc::channel::<StateError>(4);
    let writer = tokio::spawn(persist_worker(snap_rx, fd_path.clone(), err_tx));

    let mut persist_tick = tokio::time::interval(PERSIST_GAP);
    persist_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut progress_tick = tokio::time::interval(config.progress_emit_gap.max(MIN_PROGRESS_GAP));
    progress_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

    let mut engine_ended = false;
    while !engine_ended {
        tokio::select! {
            biased;
            () = token.cancelled() => {
                res.abort();
                break;
            }
            event = res.recv() => match event {
                Ok(event) => {
                    if let fast_down::Event::PushProgress(range) = &event {
                        record.merge_progress(range.clone());
                    }
                    forward(tx, event);
                }
                Err(_) => engine_ended = true,
            },
            _ = persist_tick.tick() => {
                // Bounded to one snapshot: a slower-than-one-second disk
                // skips a tick instead of piling up snapshots.
                let _ = snap_tx.try_send(record.clone());
            },
            error = err_rx.recv() => {
                if let Some(error) = error {
                    let _ = tx.send(Event::StateSaveError(error));
                }
            },
            _ = progress_tick.tick() => {
                let elapsed = loaded_elapsed.saturating_add(start.elapsed());
                let sample = progress::sample(
                    record.progress(),
                    info.size,
                    elapsed,
                    Instant::now(),
                    Some(&mut rate),
                );
                let _ = tx.send(Event::Progress(sample));
            }
        }
    }

    // 4. Stop the writer, flush the final state, emit one terminal sample.
    drop(snap_tx);
    let _ = writer.await;
    let elapsed = loaded_elapsed.saturating_add(start.elapsed());
    record.elapsed = elapsed;
    if let Err(e) = record.store(&fd_path).await {
        let _ = tx.send(Event::StateSaveError(e));
    }
    let sample = progress::sample(record.progress(), info.size, elapsed, Instant::now(), None);
    let _ = tx.send(Event::Progress(sample));

    if token.is_cancelled() {
        return TerminationReason::Cancelled;
    }
    if !record.is_complete(info.size) {
        return TerminationReason::Incomplete;
    }

    // 5. Rename the finished `.part` into place and drop the `.fd`.
    let target = if config.overwrite {
        final_path
    } else {
        match path_helper::tokio::gen_unique_path(&final_path).await {
            Ok(path) => path,
            Err(e) => {
                let _ = tx.send(Event::GenPathError(e));
                return TerminationReason::Failed;
            }
        }
    };
    if let Err(e) = fs::rename(&tmp_path, &target).await {
        if !config.overwrite {
            let _ = fs::remove_file(&target).await;
        }
        let _ = tx.send(Event::RenameFailed(e));
        return TerminationReason::Failed;
    }
    let _ = fs::remove_file(&fd_path).await;
    let _ = tx.send(Event::Renamed(target));
    TerminationReason::Completed
}

/// Persist snapshots until the sender is dropped. Only holds `Send` types, so
/// it can run on any runtime thread; failures go back over `err_tx`.
async fn persist_worker(
    mut rx: mpsc::Receiver<Record>,
    fd_path: PathBuf,
    err_tx: mpsc::Sender<StateError>,
) {
    while let Some(record) = rx.recv().await {
        if let Err(e) = record.store(&fd_path).await {
            let _ = err_tx.send(e).await;
        }
    }
}

/// Forward one engine event as the matching public event.
fn forward<PE, WE>(tx: &Tx, event: fast_down::Event<PE, WE>)
where
    PE: std::error::Error + Send + Sync + 'static,
    WE: Display,
{
    let _ = match event {
        fast_down::Event::Pulling(id) => tx.send(Event::Pulling(id)),
        fast_down::Event::PullError(id, e) => tx.send(Event::PullError(id, anyhow::Error::new(e))),
        fast_down::Event::PullTimeout(id) => tx.send(Event::PullTimeout(id)),
        fast_down::Event::PullProgress(id, range) => tx.send(Event::PullProgress(id, range)),
        fast_down::Event::Pushing(id, range) => tx.send(Event::Pushing(id, range)),
        fast_down::Event::PushError(id, range, e) => {
            tx.send(Event::PushError(id, range, anyhow::anyhow!(e.to_string())))
        }
        fast_down::Event::PushProgress(range) => tx.send(Event::PushProgress(range)),
        fast_down::Event::Flushing => tx.send(Event::Flushing),
        fast_down::Event::FlushError(e) => {
            tx.send(Event::FlushError(anyhow::anyhow!(e.to_string())))
        }
        fast_down::Event::Finished(id) => tx.send(Event::Finished(id)),
    };
}

/// A clean record at the caller's configuration, carrying `info`'s identity and
/// no progress.
fn fresh_over(record: &Record, origin_config: &PartialConfig, info: &UrlInfo) -> Record {
    let mut fresh = Record::from_url(record.url.clone(), origin_config.clone());
    fresh.refresh_identity(info);
    fresh
}

/// Atomically claim a free `.fd`/`.part` pair next to `final_path`.
///
/// Walks `name`, `name (1)`, `name (2)` … and takes the first stem where both
/// files can be created exclusively. `overwrite` plays no part here: the scratch
/// pair is always unique, and uniquification of the *final* name happens at
/// rename time.
async fn claim_pair(final_path: &Path, tx: &Tx) -> Result<(PathBuf, PathBuf), TerminationReason> {
    for base in final_path.iter_stem() {
        let fd_path = base.with_added_extension("fd");
        let tmp_path = tmp_path_for(&fd_path);
        match open_create_new().open(&tmp_path).await {
            Ok(_) => match open_create_new().open(&fd_path).await {
                Ok(_) => return Ok((fd_path, tmp_path)),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    let _ = fs::remove_file(&tmp_path).await;
                }
                Err(e) => {
                    let _ = fs::remove_file(&tmp_path).await;
                    let _ = tx.send(Event::BuildPusherError(e));
                    return Err(TerminationReason::Failed);
                }
            },
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => {
                let _ = tx.send(Event::BuildPusherError(e));
                return Err(TerminationReason::Failed);
            }
        }
    }
    unreachable!("iter_stem is infinite")
}

/// Create `final_path`'s parent directory, reporting a failure as
/// [`Event::BuildPusherError`].
async fn ensure_parent(final_path: &Path, tx: &Tx) -> bool {
    if let Some(parent) = final_path.parent()
        && let Err(e) = fs::create_dir_all(parent).await
    {
        let _ = tx.send(Event::BuildPusherError(e));
        return false;
    }
    true
}
